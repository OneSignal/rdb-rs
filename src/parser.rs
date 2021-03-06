use byteorder::{BigEndian, LittleEndian, ReadBytesExt};
use std::io::{Cursor, Read};
use std::{f64, str};

use crate::filter::Filter;
use crate::formatter::Formatter;
use crate::helper;
use crate::helper::read_exact;

#[doc(hidden)]
use crate::constants::{constant, encoding, encoding_type, op_code, version};

#[doc(hidden)]
pub use crate::types::{
    EncodingType, /* error and result types */
    RdbError, RdbOk, RdbResult, Type, ZiplistEntry,
};

pub struct RdbParser<R: Read, F: Formatter, L: Filter> {
    input: R,
    formatter: F,
    filter: L,
    last_expiretime: Option<u64>,
}

#[inline]
fn other_error(desc: impl Into<String>) -> RdbError {
    RdbError::Other(desc.into())
}

pub fn read_length_with_encoding<R: Read>(input: &mut R) -> RdbResult<(u32, bool)> {
    let length;
    let mut is_encoded = false;

    let enc_type = input.read_u8()?;

    match (enc_type & 0xC0) >> 6 {
        constant::RDB_ENCVAL => {
            is_encoded = true;
            length = (enc_type & 0x3F) as u32;
        }
        constant::RDB_6BITLEN => {
            length = (enc_type & 0x3F) as u32;
        }
        constant::RDB_14BITLEN => {
            let next_byte = input.read_u8()?;
            length = (((enc_type & 0x3F) as u32) << 8) | next_byte as u32;
        }
        _ => {
            length = input.read_u32::<BigEndian>()?;
        }
    }

    Ok((length, is_encoded))
}

pub fn read_length<R: Read>(input: &mut R) -> RdbResult<u32> {
    let (length, _) = read_length_with_encoding(input)?;
    Ok(length)
}

pub fn verify_magic<R: Read>(input: &mut R) -> RdbOk {
    let mut magic = [0; 5];
    if input.read(&mut magic)? != 5 {
        return Err(other_error("Could not read enough bytes for the magic"));
    }

    if magic == constant::RDB_MAGIC.as_bytes() {
        Ok(())
    } else {
        Err(other_error("Invalid magic string"))
    }
}

pub fn verify_version<R: Read>(input: &mut R) -> RdbOk {
    let mut version = [0; 4];
    if input.read(&mut version)? != 4 {
        return Err(other_error("Could not read enough bytes for the version"));
    }

    let version = (version[0] - 48) as u32 * 1000
        + (version[1] - 48) as u32 * 100
        + (version[2] - 48) as u32 * 10
        + (version[3] - 48) as u32;

    let is_ok = version >= version::SUPPORTED_MINIMUM && version <= version::SUPPORTED_MAXIMUM;

    if is_ok {
        Ok(())
    } else {
        Err(other_error(format!(
            "Version {} RDB files are not supported. Supported versions are {}-{}",
            version,
            version::SUPPORTED_MINIMUM,
            version::SUPPORTED_MAXIMUM
        )))
    }
}

pub fn read_blob<R: Read>(input: &mut R) -> RdbResult<Vec<u8>> {
    let (length, is_encoded) = read_length_with_encoding(input)?;

    if is_encoded {
        let result = match length {
            encoding::INT8 => helper::int_to_vec(input.read_i8()? as i32),
            encoding::INT16 => helper::int_to_vec(input.read_i16::<LittleEndian>()? as i32),
            encoding::INT32 => helper::int_to_vec(input.read_i32::<LittleEndian>()? as i32),
            encoding::LZF => {
                let compressed_length = read_length(input)?;
                let real_length = read_length(input)?;
                let data = read_exact(input, compressed_length as usize)?;
                lzf::decompress(&data, real_length as usize).unwrap()
            }
            _ => panic!("Unknown encoding: {}", length),
        };

        Ok(result)
    } else {
        read_exact(input, length as usize)
    }
}

fn read_ziplist_metadata<T: Read>(input: &mut T) -> RdbResult<(u32, u32, u16)> {
    let zlbytes = input.read_u32::<LittleEndian>()?;
    let zltail = input.read_u32::<LittleEndian>()?;
    let zllen = input.read_u16::<LittleEndian>()?;

    Ok((zlbytes, zltail, zllen))
}

impl<R: Read, F: Formatter, L: Filter> RdbParser<R, F, L> {
    pub fn new(input: R, formatter: F, filter: L) -> RdbParser<R, F, L> {
        RdbParser {
            input,
            formatter,
            filter,
            last_expiretime: None,
        }
    }

    pub fn parse(&mut self) -> RdbOk {
        verify_magic(&mut self.input)?;
        verify_version(&mut self.input)?;

        self.formatter.start_rdb()?;

        let mut last_database: u32 = 0;
        loop {
            let next_op = self.input.read_u8()?;

            match next_op {
                op_code::SELECTDB => {
                    last_database = unwrap_or_panic!(read_length(&mut self.input));
                    if self.filter.matches_db(last_database) {
                        self.formatter.start_database(last_database)?;
                    }
                }
                op_code::EOF => {
                    self.formatter.end_database(last_database)?;
                    self.formatter.end_rdb()?;

                    let mut checksum = Vec::new();
                    let len = self.input.read_to_end(&mut checksum)?;
                    if len > 0 {
                        self.formatter.checksum(&checksum)?;
                    }
                    break;
                }
                op_code::EXPIRETIME_MS => {
                    let expiretime_ms = self.input.read_u64::<LittleEndian>()?;
                    self.last_expiretime = Some(expiretime_ms);
                }
                op_code::EXPIRETIME => {
                    let expiretime = self.input.read_u32::<BigEndian>()?;
                    self.last_expiretime = Some(expiretime as u64 * 1000);
                }
                op_code::RESIZEDB => {
                    let db_size = read_length(&mut self.input)?;
                    let expires_size = read_length(&mut self.input)?;

                    self.formatter.resizedb(db_size, expires_size)?;
                }
                op_code::AUX => {
                    let auxkey = read_blob(&mut self.input)?;
                    let auxval = read_blob(&mut self.input)?;

                    self.formatter.aux_field(&auxkey, &auxval)?;
                }
                _ => {
                    if self.filter.matches_db(last_database) {
                        let key = read_blob(&mut self.input)?;

                        if self.filter.matches_type(next_op) && self.filter.matches_key(&key) {
                            self.read_type(&key, next_op)?;
                        } else {
                            self.skip_object(next_op)?;
                        }
                    } else {
                        self.skip_key_and_object(next_op)?;
                    }

                    self.last_expiretime = None;
                }
            }
        }

        Ok(())
    }

    fn read_linked_list(&mut self, key: &[u8], typ: Type) -> RdbOk {
        let mut len = read_length(&mut self.input)?;

        match typ {
            Type::List => {
                self.formatter.start_list(
                    key,
                    len,
                    self.last_expiretime,
                    EncodingType::LinkedList,
                )?;
            }
            Type::Set => {
                self.formatter.start_set(
                    key,
                    len,
                    self.last_expiretime,
                    EncodingType::LinkedList,
                )?;
            }
            _ => panic!("Unknown encoding type for linked list"),
        }

        while len > 0 {
            let blob = read_blob(&mut self.input)?;
            self.formatter.list_element(key, &blob)?;
            len -= 1;
        }

        match typ {
            Type::List => self.formatter.end_list(key)?,
            Type::Set => self.formatter.end_set(key)?,
            _ => panic!("Unknown encoding type for linked list"),
        }

        Ok(())
    }

    fn read_sorted_set_type_2(&mut self, key: &[u8]) -> RdbOk {
        let mut set_items = unwrap_or_panic!(read_length(&mut self.input));

        self.formatter.start_sorted_set(
            key,
            set_items,
            self.last_expiretime,
            EncodingType::Hashtable,
        )?;

        while set_items > 0 {
            let val = read_blob(&mut self.input)?;

            let score = self.input.read_f64::<LittleEndian>()?;

            self.formatter.sorted_set_element(key, score, &val)?;

            set_items -= 1;
        }

        self.formatter.end_sorted_set(key)?;

        Ok(())
    }

    fn read_sorted_set(&mut self, key: &[u8]) -> RdbOk {
        let mut set_items = unwrap_or_panic!(read_length(&mut self.input));

        self.formatter.start_sorted_set(
            key,
            set_items,
            self.last_expiretime,
            EncodingType::Hashtable,
        )?;

        while set_items > 0 {
            let val = read_blob(&mut self.input)?;
            let score_length = self.input.read_u8()?;
            let score = match score_length {
                253 => f64::NAN,
                254 => f64::INFINITY,
                255 => f64::NEG_INFINITY,
                _ => {
                    let tmp = read_exact(&mut self.input, score_length as usize)?;
                    unsafe { str::from_utf8_unchecked(&tmp) }
                        .parse::<f64>()
                        .unwrap()
                }
            };

            self.formatter.sorted_set_element(key, score, &val)?;

            set_items -= 1;
        }

        self.formatter.end_sorted_set(key)?;

        Ok(())
    }

    fn read_hash(&mut self, key: &[u8]) -> RdbOk {
        let mut hash_items = read_length(&mut self.input)?;

        self.formatter.start_hash(
            key,
            hash_items,
            self.last_expiretime,
            EncodingType::Hashtable,
        )?;

        while hash_items > 0 {
            let field = read_blob(&mut self.input)?;
            let val = read_blob(&mut self.input)?;

            self.formatter.hash_element(key, &field, &val)?;

            hash_items -= 1;
        }

        self.formatter.end_hash(key)?;

        Ok(())
    }

    fn read_ziplist_entry<T: Read>(&mut self, ziplist: &mut T) -> RdbResult<ZiplistEntry> {
        // 1. 1 or 5 bytes length of previous entry
        let byte = ziplist.read_u8()?;
        if byte == 254 {
            let mut bytes = [0; 4];
            if ziplist.read(&mut bytes)? != 4 {
                return Err(other_error(
                    "Could not read 4 bytes to skip after ziplist length",
                ));
            }
        }

        let length: u64;
        let number_value: i64;

        // 2. Read flag or number value
        let flag = ziplist.read_u8()?;

        match (flag & 0xC0) >> 6 {
            0 => length = (flag & 0x3F) as u64,
            1 => {
                let next_byte = ziplist.read_u8()?;
                length = (((flag & 0x3F) as u64) << 8) | next_byte as u64;
            }
            2 => {
                length = ziplist.read_u32::<BigEndian>()? as u64;
            }
            _ => {
                match (flag & 0xF0) >> 4 {
                    0xC => number_value = ziplist.read_i16::<LittleEndian>()? as i64,
                    0xD => number_value = ziplist.read_i32::<LittleEndian>()? as i64,
                    0xE => number_value = ziplist.read_i64::<LittleEndian>()? as i64,
                    0xF => match flag & 0xF {
                        0 => {
                            let mut bytes = [0; 3];
                            if ziplist.read(&mut bytes)? != 3 {
                                return Err(other_error(
                                    "Could not read enough bytes for 24bit number",
                                ));
                            }

                            let number: i32 = (((bytes[2] as i32) << 24)
                                ^ ((bytes[1] as i32) << 16)
                                ^ ((bytes[0] as i32) << 8)
                                ^ 48)
                                >> 8;

                            number_value = number as i64;
                        }
                        0xE => {
                            number_value = ziplist.read_i8()? as i64;
                        }
                        _ => {
                            number_value = (flag & 0xF) as i64 - 1;
                        }
                    },
                    _ => {
                        panic!("Flag not handled: {}", flag);
                    }
                }

                return Ok(ZiplistEntry::Number(number_value));
            }
        }

        // 3. Read value
        let rawval = read_exact(ziplist, length as usize)?;
        Ok(ZiplistEntry::String(rawval))
    }

    fn read_ziplist_entry_string<T: Read>(&mut self, reader: &mut T) -> RdbResult<Vec<u8>> {
        let entry = self.read_ziplist_entry(reader)?;
        match entry {
            ZiplistEntry::String(val) => Ok(val),
            ZiplistEntry::Number(val) => Ok(val.to_string().into_bytes()),
        }
    }

    fn read_list_ziplist(&mut self, key: &[u8]) -> RdbOk {
        let ziplist = read_blob(&mut self.input)?;
        let raw_length = ziplist.len() as u64;

        let mut reader = Cursor::new(ziplist);
        let (_zlbytes, _zltail, zllen) = read_ziplist_metadata(&mut reader)?;

        self.formatter.start_list(
            key,
            zllen as u32,
            self.last_expiretime,
            EncodingType::Ziplist(raw_length),
        )?;

        for _ in 0..zllen {
            let entry = self.read_ziplist_entry_string(&mut reader)?;
            self.formatter.list_element(key, &entry)?;
        }

        let last_byte = reader.read_u8()?;
        if last_byte != 0xFF {
            return Err(other_error("Invalid end byte of ziplist"));
        }

        self.formatter.end_list(key)?;

        Ok(())
    }

    fn read_hash_ziplist(&mut self, key: &[u8]) -> RdbOk {
        let ziplist = read_blob(&mut self.input)?;
        let raw_length = ziplist.len() as u64;

        let mut reader = Cursor::new(ziplist);
        let (_zlbytes, _zltail, zllen) = read_ziplist_metadata(&mut reader)?;

        assert!(zllen % 2 == 0);
        let zllen = zllen / 2;

        self.formatter.start_hash(
            key,
            zllen as u32,
            self.last_expiretime,
            EncodingType::Ziplist(raw_length),
        )?;

        for _ in 0..zllen {
            let field = self.read_ziplist_entry_string(&mut reader)?;
            let value = self.read_ziplist_entry_string(&mut reader)?;
            self.formatter.hash_element(key, &field, &value)?;
        }

        let last_byte = reader.read_u8()?;
        if last_byte != 0xFF {
            return Err(other_error("Invalid end byte of ziplist"));
        }

        self.formatter.end_hash(key)?;

        Ok(())
    }

    fn read_sortedset_ziplist(&mut self, key: &[u8]) -> RdbOk {
        let ziplist = read_blob(&mut self.input)?;
        let raw_length = ziplist.len() as u64;

        let mut reader = Cursor::new(ziplist);
        let (_zlbytes, _zltail, zllen) = read_ziplist_metadata(&mut reader)?;

        self.formatter.start_sorted_set(
            key,
            zllen as u32,
            self.last_expiretime,
            EncodingType::Ziplist(raw_length),
        )?;

        assert!(zllen % 2 == 0);
        let zllen = zllen / 2;

        for _ in 0..zllen {
            let entry = self.read_ziplist_entry_string(&mut reader)?;
            let score = self.read_ziplist_entry_string(&mut reader)?;
            let score = str::from_utf8(&score).unwrap().parse::<f64>().unwrap();
            self.formatter.sorted_set_element(key, score, &entry)?;
        }

        let last_byte = reader.read_u8()?;
        if last_byte != 0xFF {
            return Err(other_error("Invalid end byte of ziplist"));
        }

        self.formatter.end_sorted_set(key)?;

        Ok(())
    }

    fn read_quicklist_ziplist(&mut self, key: &[u8]) -> RdbOk {
        let ziplist = read_blob(&mut self.input)?;

        let mut reader = Cursor::new(ziplist);
        let (_zlbytes, _zltail, zllen) = read_ziplist_metadata(&mut reader)?;

        for _ in 0..zllen {
            let entry = self.read_ziplist_entry_string(&mut reader)?;
            self.formatter.list_element(key, &entry)?;
        }

        let last_byte = reader.read_u8()?;
        if last_byte != 0xFF {
            return Err(other_error("Invalid end byte of ziplist (quicklist)"));
        }

        Ok(())
    }

    fn read_zipmap_entry<T: Read>(&mut self, next_byte: u8, zipmap: &mut T) -> RdbResult<Vec<u8>> {
        let elem_len;
        match next_byte {
            253 => elem_len = zipmap.read_u32::<LittleEndian>().unwrap(),
            254 | 255 => panic!("Invalid length value in zipmap: {}", next_byte),
            _ => elem_len = next_byte as u32,
        }

        read_exact(zipmap, elem_len as usize)
    }

    fn read_hash_zipmap(&mut self, key: &[u8]) -> RdbOk {
        let zipmap = read_blob(&mut self.input)?;
        let raw_length = zipmap.len() as u64;

        let mut reader = Cursor::new(zipmap);

        let zmlen = reader.read_u8()?;

        let mut length: i32;
        let size;
        if zmlen <= 254 {
            length = zmlen as i32;
            size = zmlen
        } else {
            length = -1;
            size = 0;
        }

        self.formatter.start_hash(
            key,
            size as u32,
            self.last_expiretime,
            EncodingType::Zipmap(raw_length),
        )?;

        loop {
            let next_byte = reader.read_u8()?;

            if next_byte == 0xFF {
                break; // End of list.
            }

            let field = self.read_zipmap_entry(next_byte, &mut reader)?;

            let next_byte = reader.read_u8()?;
            let _free = reader.read_u8()?;
            let value = self.read_zipmap_entry(next_byte, &mut reader)?;

            self.formatter.hash_element(key, &field, &value)?;

            if length > 0 {
                length -= 1;
            }

            if length == 0 {
                let last_byte = reader.read_u8()?;

                if last_byte != 0xFF {
                    return Err(other_error("Invalid end byte of zipmap"));
                }
                break;
            }
        }

        self.formatter.end_hash(key)?;

        Ok(())
    }

    fn read_set_intset(&mut self, key: &[u8]) -> RdbOk {
        let intset = read_blob(&mut self.input)?;
        let raw_length = intset.len() as u64;

        let mut reader = Cursor::new(intset);
        let byte_size = reader.read_u32::<LittleEndian>()?;
        let intset_length = reader.read_u32::<LittleEndian>()?;

        self.formatter.start_set(
            key,
            intset_length,
            self.last_expiretime,
            EncodingType::Intset(raw_length),
        )?;

        for _ in 0..intset_length {
            let val = match byte_size {
                2 => reader.read_i16::<LittleEndian>()? as i64,
                4 => reader.read_i32::<LittleEndian>()? as i64,
                8 => reader.read_i64::<LittleEndian>()?,
                _ => panic!("unhandled byte size in intset: {}", byte_size),
            };

            self.formatter
                .set_element(key, val.to_string().as_bytes())?;
        }

        self.formatter.end_set(key)?;

        Ok(())
    }

    fn read_quicklist(&mut self, key: &[u8]) -> RdbOk {
        let len = read_length(&mut self.input)?;

        self.formatter
            .start_set(key, 0, self.last_expiretime, EncodingType::Quicklist)?;
        for _ in 0..len {
            self.read_quicklist_ziplist(key)?;
        }
        self.formatter.end_set(key)?;

        Ok(())
    }

    fn read_type(&mut self, key: &[u8], value_type: u8) -> RdbOk {
        match value_type {
            encoding_type::STRING => {
                let val = read_blob(&mut self.input)?;
                self.formatter.set(key, &val, self.last_expiretime)?;
            }
            encoding_type::LIST => self.read_linked_list(key, Type::List)?,
            encoding_type::SET => self.read_linked_list(key, Type::Set)?,
            encoding_type::ZSET => self.read_sorted_set(key)?,
            encoding_type::ZSET_2 => self.read_sorted_set_type_2(key)?,
            encoding_type::HASH => self.read_hash(key)?,
            encoding_type::HASH_ZIPMAP => self.read_hash_zipmap(key)?,
            encoding_type::LIST_ZIPLIST => self.read_list_ziplist(key)?,
            encoding_type::SET_INTSET => self.read_set_intset(key)?,
            encoding_type::ZSET_ZIPLIST => self.read_sortedset_ziplist(key)?,
            encoding_type::HASH_ZIPLIST => self.read_hash_ziplist(key)?,
            encoding_type::LIST_QUICKLIST => self.read_quicklist(key)?,
            _ => panic!("Value Type not implemented: {}", value_type),
        };

        Ok(())
    }

    fn skip(&mut self, skip_bytes: usize) -> RdbResult<()> {
        let mut buf = vec![0; skip_bytes];
        self.input.read_exact(&mut buf)?;

        Ok(())
    }

    fn skip_blob(&mut self) -> RdbResult<()> {
        let (len, is_encoded) = unwrap_or_panic!(read_length_with_encoding(&mut self.input));
        let skip_bytes;

        if is_encoded {
            skip_bytes = match len {
                encoding::INT8 => 1,
                encoding::INT16 => 2,
                encoding::INT32 => 4,
                encoding::LZF => {
                    let compressed_length = unwrap_or_panic!(read_length(&mut self.input));
                    let _real_length = unwrap_or_panic!(read_length(&mut self.input));
                    compressed_length
                }
                _ => panic!("Unknown encoding: {}", len),
            }
        } else {
            skip_bytes = len;
        }

        self.skip(skip_bytes as usize)
    }

    fn skip_object(&mut self, enc_type: u8) -> RdbResult<()> {
        let blobs_to_skip = match enc_type {
            encoding_type::STRING
            | encoding_type::HASH_ZIPMAP
            | encoding_type::LIST_ZIPLIST
            | encoding_type::SET_INTSET
            | encoding_type::ZSET_ZIPLIST
            | encoding_type::HASH_ZIPLIST => 1,
            encoding_type::LIST | encoding_type::SET | encoding_type::LIST_QUICKLIST => {
                unwrap_or_panic!(read_length(&mut self.input))
            }
            encoding_type::ZSET | encoding_type::HASH => {
                unwrap_or_panic!(read_length(&mut self.input)) * 2
            }
            encoding_type::ZSET_2 => {
                let length = read_length(&mut self.input)?;
                for _ in 0..length {
                    self.skip_blob()?;
                    self.skip(8)?;
                }

                0
            }
            _ => panic!("Unknown encoding type: {}", enc_type),
        };

        for _ in 0..blobs_to_skip {
            self.skip_blob()?
        }

        Ok(())
    }

    fn skip_key_and_object(&mut self, enc_type: u8) -> RdbResult<()> {
        self.skip_blob()?;
        self.skip_object(enc_type)?;
        Ok(())
    }
}
