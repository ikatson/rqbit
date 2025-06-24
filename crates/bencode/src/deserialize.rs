use arrayvec::ArrayVec;
use buffers::ByteBuf;
use memchr::memchr;

pub struct BencodeDeserializer<'de> {
    buf: &'de [u8],
    field_context: ErrorContext<'de>,
    field_context_did_not_fit: usize,
    parsing_key: bool,

    // This is a f**ing hack
    pub is_torrent_info: bool,
    pub torrent_info_digest: Option<[u8; 20]>,
    pub torrent_info_bytes: Option<&'de [u8]>,
}

impl<'de> BencodeDeserializer<'de> {
    pub fn new_from_buf(buf: &'de [u8]) -> BencodeDeserializer<'de> {
        Self {
            buf,
            field_context: Default::default(),
            field_context_did_not_fit: 0,
            parsing_key: false,
            is_torrent_info: false,
            torrent_info_digest: None,
            torrent_info_bytes: None,
        }
    }
    pub fn into_remaining(self) -> &'de [u8] {
        self.buf
    }

    pub fn parse_first_byte(&mut self, c: u8, err: Error) -> Result<(), Error> {
        match self.buf.first() {
            Some(start) if *start == c => {
                self.buf = &self.buf[1..];
                Ok(())
            }
            Some(_) => Err(err),
            None => Err(Error::Eof),
        }
    }

    fn parse_integer(&mut self) -> Result<i64, Error> {
        match memchr(b'e', self.buf) {
            Some(end) => {
                let intbytes = &self.buf[..end];
                let value: i64 = atoi::atoi(intbytes).ok_or(Error::InvalidValue)?;
                let rem = &self.buf[end + 1..];
                self.buf = rem;
                Ok(value)
            }
            None => Err(Error::Eof),
        }
    }

    fn parse_bytes(&mut self) -> Result<&'de [u8], Error> {
        let b = match memchr(b':', self.buf) {
            Some(length_delim) => {
                let lenbytes = &self.buf[..length_delim];
                let length: usize = atoi::atoi(lenbytes).ok_or(Error::InvalidBytesNotInt)?;
                let bytes_start = length_delim + 1;
                let bytes_end = bytes_start + length;
                let bytes = self.buf.get(bytes_start..bytes_end).ok_or(Error::Eof)?;
                self.buf = &self.buf[bytes_end..];
                bytes
            }
            None if self.buf.is_empty() => return Err(Error::Eof),
            None => return Err(Error::InvalidBytesNoColon),
        };
        if self.parsing_key && self.field_context.try_push(ByteBuf(b)).is_err() {
            self.field_context_did_not_fit += 1;
        }
        Ok(b)
    }
}

pub fn from_bytes<'a, T>(buf: &'a [u8]) -> Result<T, ErrorWithContext<'a>>
where
    T: serde::de::Deserialize<'a>,
{
    let mut de = BencodeDeserializer::new_from_buf(buf);
    let v = match T::deserialize(&mut de) {
        Ok(v) => v,
        Err(e) => {
            return Err(ErrorWithContext {
                kind: e,
                ctx: de.field_context,
            });
        }
    };
    if !de.buf.is_empty() {
        return Err(ErrorWithContext {
            kind: Error::BytesRemaining(de.buf.len()),
            ctx: de.field_context,
        });
    }
    Ok(v)
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("not supported by bencode")]
    NotSupported,
    #[error("{0}")]
    Custom(Box<String>), // box to reduce size
    #[error("expected 0 or 1 for boolean, got {0}")]
    InvalidBool(i64),
    #[error("deserialized successfully, but {0} bytes remaining")]
    BytesRemaining(usize),
    #[error("invalid length: {0}")]
    InvalidLength(usize),
    #[error("invalid value")]
    InvalidValue,
    #[error("expected int_len:data, but int_len is not an integer")]
    InvalidBytesNotInt,
    #[error("expected int_len:data, but \":\" not found")]
    InvalidBytesNoColon,
    #[error("invalid utf-8")]
    InvalidUtf8,
    #[error("eof")]
    Eof,
}

type ErrorContext<'de> = ArrayVec<ByteBuf<'de>, 4>;

#[derive(Debug)]
pub struct ErrorWithContext<'de> {
    kind: Error,
    ctx: ErrorContext<'de>,
}

impl ErrorWithContext<'_> {
    pub fn kind(&self) -> &Error {
        &self.kind
    }

    pub fn into_kind(self) -> Error {
        self.kind
    }

    pub fn into_anyhow(self) -> anyhow::Error {
        anyhow::Error::new(self.kind).context(format!("{:?}", self.ctx))
    }
}

impl std::fmt::Display for ErrorWithContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (id, ctx_key) in self.ctx.iter().copied().enumerate() {
            if id > 0 {
                write!(f, " -> {:?}", ctx_key)?;
            } else {
                write!(f, "{:?}", ctx_key)?;
            }
        }
        if !self.ctx.is_empty() {
            write!(f, ": ")?;
        }
        write!(f, "{}", self.kind)
    }
}

impl std::error::Error for ErrorWithContext<'_> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.kind)
    }
}

impl serde::de::Error for Error {
    fn custom<T>(msg: T) -> Self
    where
        T: std::fmt::Display,
    {
        Self::Custom(Box::new(msg.to_string()))
    }
}

impl<'de> serde::de::Deserializer<'de> for &mut BencodeDeserializer<'de> {
    type Error = Error;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        match self.buf.first().copied() {
            Some(b'd') => self.deserialize_map(visitor),
            Some(b'i') => self.deserialize_u64(visitor),
            Some(b'l') => self.deserialize_seq(visitor),
            Some(_) => self.deserialize_bytes(visitor),
            None => Err(Error::Eof),
        }
    }

    fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.parse_first_byte(b'i', Error::InvalidValue)?;
        let value = self.parse_integer()?;
        if value > 1 {
            return Err(Error::InvalidBool(value));
        }
        visitor.visit_bool(value == 1)
    }

    fn deserialize_i8<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i16<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_i64<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.parse_first_byte(b'i', Error::InvalidValue)?;
        visitor.visit_i64(self.parse_integer()?)
    }

    fn deserialize_u8<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_u16<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_u32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_u64<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_i64(visitor)
    }

    fn deserialize_f32<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(Error::NotSupported)
    }

    fn deserialize_f64<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(Error::NotSupported)
    }

    fn deserialize_char<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(Error::NotSupported)
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let b = self.parse_bytes()?;
        let s = std::str::from_utf8(b).map_err(|_| Error::InvalidUtf8)?;
        visitor.visit_borrowed_str(s)
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let b = self.parse_bytes()?;
        visitor.visit_borrowed_bytes(b)
    }

    fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_some(&mut *self)
    }

    fn deserialize_unit<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(Error::NotSupported)
    }

    fn deserialize_unit_struct<V>(
        self,
        _name: &'static str,
        _visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(Error::NotSupported)
    }

    fn deserialize_newtype_struct<V>(
        self,
        _name: &'static str,
        _visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(Error::NotSupported)
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.parse_first_byte(b'l', Error::InvalidValue)?;
        visitor.visit_seq(SeqAccess { de: self })
    }

    fn deserialize_tuple<V>(self, _len: usize, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.parse_first_byte(b'd', Error::InvalidValue)?;
        visitor.visit_map(MapAccess { de: self })
    }

    fn deserialize_struct<V>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(Error::NotSupported)
    }

    fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_borrowed_bytes(self.parse_bytes()?)
    }

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }
}

struct MapAccess<'a, 'de> {
    de: &'a mut BencodeDeserializer<'de>,
}

struct SeqAccess<'a, 'de> {
    de: &'a mut BencodeDeserializer<'de>,
}

impl<'de> serde::de::MapAccess<'de> for MapAccess<'_, 'de> {
    type Error = Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: serde::de::DeserializeSeed<'de>,
    {
        if self.de.buf.starts_with(b"e") {
            self.de.buf = &self.de.buf[1..];
            return Ok(None);
        }
        self.de.parsing_key = true;
        let retval = seed.deserialize(&mut *self.de)?;
        self.de.parsing_key = false;
        Ok(Some(retval))
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::DeserializeSeed<'de>,
    {
        #[cfg(any(feature = "sha1-crypto-hash", feature = "sha1-ring"))]
        let buf_before = self.de.buf;
        let value = seed.deserialize(&mut *self.de)?;
        #[cfg(any(feature = "sha1-crypto-hash", feature = "sha1-ring"))]
        {
            use sha1w::{ISha1, Sha1};
            if self.de.is_torrent_info && self.de.field_context.as_slice() == [ByteBuf(b"info")] {
                let len = self.de.buf.as_ptr() as usize - buf_before.as_ptr() as usize;
                let mut hash = Sha1::new();
                let torrent_info_bytes = &buf_before[..len];
                hash.update(torrent_info_bytes);
                let digest = hash.finish();
                self.de.torrent_info_digest = Some(digest);
                self.de.torrent_info_bytes = Some(torrent_info_bytes);
            }
        }
        if self.de.field_context_did_not_fit > 0 {
            self.de.field_context_did_not_fit -= 1;
        } else {
            self.de.field_context.pop();
        }
        Ok(value)
    }
}

impl<'de> serde::de::SeqAccess<'de> for SeqAccess<'_, 'de> {
    type Error = Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: serde::de::DeserializeSeed<'de>,
    {
        if self.de.buf.starts_with(b"e") {
            self.de.buf = &self.de.buf[1..];
            return Ok(None);
        }
        Ok(Some(seed.deserialize(&mut *self.de)?))
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use crate::from_bytes;

    #[test]
    fn test_deserialize_error_context() {
        #[derive(Deserialize, Debug)]
        struct Child {
            #[allow(unused)]
            key: u64,
        }
        #[derive(Deserialize, Debug)]
        struct Parent {
            #[allow(unused)]
            child: Child,
        }

        let e = from_bytes::<Parent>(&b"d5:childd3:key2:hiee"[..]).expect_err("expected error");
        println!("{e:#}")
    }
}
