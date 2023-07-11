use buffers::ByteBuf;
use serde::de::Error as DeError;
use sha1w::{ISha1, Sha1};

pub struct BencodeDeserializer<'de> {
    buf: &'de [u8],
    field_context: Vec<ByteBuf<'de>>,
    parsing_key: bool,

    // This is a f**ing hack
    pub is_torrent_info: bool,
    pub torrent_info_digest: Option<[u8; 20]>,
}

impl<'de> BencodeDeserializer<'de> {
    pub fn new_from_buf(buf: &'de [u8]) -> BencodeDeserializer<'de> {
        Self {
            buf,
            field_context: Default::default(),
            parsing_key: false,
            is_torrent_info: false,
            torrent_info_digest: None,
        }
    }
    pub fn into_remaining(self) -> &'de [u8] {
        self.buf
    }
    fn parse_integer(&mut self) -> Result<i64, Error> {
        match self.buf.iter().copied().position(|e| e == b'e') {
            Some(end) => {
                let intbytes = &self.buf[1..end];
                let value: i64 = std::str::from_utf8(intbytes)
                    .map_err(|e| Error::new_from_err(e).set_context(self))?
                    .parse()
                    .map_err(|e| Error::new_from_err(e).set_context(self))?;
                let rem = self.buf.get(end + 1..).unwrap_or_default();
                self.buf = rem;
                Ok(value)
            }
            None => Err(Error::custom("cannot parse integer, unexpected EOF").set_context(self)),
        }
    }

    fn parse_bytes(&mut self) -> Result<&'de [u8], Error> {
        match self.buf.iter().copied().position(|e| e == b':') {
            Some(length_delim) => {
                let lenbytes = &self.buf[..length_delim];
                let length: usize = std::str::from_utf8(lenbytes)
                    .map_err(|e| Error::new_from_err(e).set_context(self))?
                    .parse()
                    .map_err(|e| Error::new_from_err(e).set_context(self))?;
                let bytes_start = length_delim + 1;
                let bytes_end = bytes_start + length;
                let bytes = &self.buf.get(bytes_start..bytes_end).ok_or_else(|| {
                    Error::custom(format!(
                        "could not get byte range {}..{}, data in the buffer: {:?}",
                        bytes_start, bytes_end, &self.buf
                    ))
                    .set_context(self)
                })?;
                let rem = self.buf.get(bytes_end..).unwrap_or_default();
                self.buf = rem;
                Ok(bytes)
            }
            None => Err(Error::custom("cannot parse bytes, unexpected EOF").set_context(self)),
        }
    }

    fn parse_bytes_checked(&mut self) -> Result<&'de [u8], Error> {
        let first = match self.buf.first().copied() {
            Some(first) => first,
            None => return Err(Error::custom("expected bencode bytes, got EOF").set_context(self)),
        };
        match first {
            b'0'..=b'9' => {}
            _ => return Err(Error::custom("expected bencode bytes").set_context(self)),
        }
        let b = self.parse_bytes()?;
        if self.parsing_key {
            self.field_context.push(ByteBuf(b));
        }
        Ok(b)
    }
}

pub fn from_bytes<'a, T>(buf: &'a [u8]) -> anyhow::Result<T>
where
    T: serde::de::Deserialize<'a>,
{
    let mut de = BencodeDeserializer::new_from_buf(buf);
    let v = T::deserialize(&mut de)?;
    if !de.buf.is_empty() {
        anyhow::bail!(
            "deserialized successfully, but {} bytes remaining",
            de.buf.len()
        )
    }
    Ok(v)
}

#[derive(Debug)]
enum ErrorKind {
    Other(anyhow::Error),
    NotSupported(&'static str),
}

#[derive(Debug, Default)]
pub struct ErrorContext {
    field_stack: Vec<String>,
}

impl std::fmt::Display for ErrorContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut it = self.field_stack.iter();
        if let Some(field) = it.next() {
            write!(f, "\"{field}\"")?;
        } else {
            return Ok(());
        }
        for field in self.field_stack.iter().skip(1) {
            write!(f, " -> \"{field}\"")?;
        }
        f.write_str(": ")
    }
}

#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    context: ErrorContext,
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorKind::Other(err) => err.fmt(f),
            ErrorKind::NotSupported(s) => write!(f, "{s} is not supported by bencode"),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.context, self.kind)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ErrorKind::Other(err) => err.source(),
            _ => None,
        }
    }
}

impl Error {
    fn new_from_err<E>(e: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Error {
            kind: ErrorKind::Other(anyhow::Error::new(e)),
            context: Default::default(),
        }
    }

    fn new_from_kind(kind: ErrorKind) -> Self {
        Self {
            kind,
            context: Default::default(),
        }
    }

    fn new_from_anyhow(e: anyhow::Error) -> Self {
        Error {
            kind: ErrorKind::Other(e),
            context: Default::default(),
        }
    }
    fn custom_with_de<M: std::fmt::Display>(msg: M, de: &BencodeDeserializer<'_>) -> Self {
        Self::custom(msg).set_context(de)
    }
    fn set_context(mut self, de: &BencodeDeserializer<'_>) -> Self {
        self.context = ErrorContext {
            field_stack: de.field_context.iter().map(|s| format!("{s}")).collect(),
        };
        self
    }
}

impl serde::de::Error for Error {
    fn custom<T>(msg: T) -> Self
    where
        T: std::fmt::Display,
    {
        Self {
            kind: ErrorKind::Other(anyhow::anyhow!("{}", msg)),
            context: Default::default(),
        }
    }
}

impl<'de, 'a> serde::de::Deserializer<'de> for &'a mut BencodeDeserializer<'de> {
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
            None => Err(Error::custom_with_de("empty input", self)),
        }
    }

    fn deserialize_bool<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(
            Error::new_from_kind(ErrorKind::NotSupported("bencode doesn't support booleans"))
                .set_context(self),
        )
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
        if !self.buf.starts_with(b"i") {
            return Err(Error::custom_with_de("expected bencode int", self));
        }
        visitor
            .visit_i64(self.parse_integer()?)
            .map_err(|e: Self::Error| e.set_context(self))
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
        Err(
            Error::new_from_kind(ErrorKind::NotSupported("bencode doesn't support floats"))
                .set_context(self),
        )
    }

    fn deserialize_f64<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(
            Error::new_from_kind(ErrorKind::NotSupported("bencode doesn't support floats"))
                .set_context(self),
        )
    }

    fn deserialize_char<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(
            Error::new_from_kind(ErrorKind::NotSupported("bencode doesn't support chars"))
                .set_context(self),
        )
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let first = match self.buf.first().copied() {
            Some(first) => first,
            None => {
                return Err(Error::custom_with_de(
                    "expected bencode string, got EOF",
                    self,
                ))
            }
        };
        match first {
            b'0'..=b'9' => {}
            _ => return Err(Error::custom_with_de("expected bencode string", self)),
        }
        let b = self.parse_bytes()?;
        let s = std::str::from_utf8(b).map_err(|e| {
            Error::new_from_anyhow(anyhow::anyhow!("error reading utf-8: {}", e)).set_context(self)
        })?;
        visitor
            .visit_borrowed_str(s)
            .map_err(|e: Self::Error| e.set_context(self))
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
        let b = self.parse_bytes_checked()?;
        visitor
            .visit_borrowed_bytes(b)
            .map_err(|e: Self::Error| e.set_context(self))
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
        visitor
            .visit_some(&mut *self)
            .map_err(|e: Self::Error| e.set_context(self))
    }

    fn deserialize_unit<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(Error::new_from_kind(ErrorKind::NotSupported(
            "bencode doesn't support unit types",
        ))
        .set_context(self))
    }

    fn deserialize_unit_struct<V>(
        self,
        _name: &'static str,
        _visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(Error::new_from_kind(ErrorKind::NotSupported(
            "bencode doesn't support unit structs",
        ))
        .set_context(self))
    }

    fn deserialize_newtype_struct<V>(
        self,
        _name: &'static str,
        _visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        Err(
            Error::new_from_kind(ErrorKind::NotSupported("bencode doesn't newtype structs"))
                .set_context(self),
        )
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        if !self.buf.starts_with(b"l") {
            return Err(Error::custom(format!(
                "expected bencode list, but got {}",
                self.buf[0] as char,
            )));
        }
        self.buf = self.buf.get(1..).unwrap_or_default();
        visitor
            .visit_seq(SeqAccess { de: self })
            .map_err(|e: Self::Error| e.set_context(self))
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
        if !self.buf.starts_with(b"d") {
            return Err(Error::custom("expected bencode dict"));
        }
        self.buf = self.buf.get(1..).unwrap_or_default();
        visitor
            .visit_map(MapAccess { de: self })
            .map_err(|e: Self::Error| e.set_context(self))
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
        Err(
            Error::new_from_kind(ErrorKind::NotSupported("deserializing enums not supported"))
                .set_context(self),
        )
    }

    fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let name = self.parse_bytes_checked()?;
        visitor
            .visit_borrowed_bytes(name)
            .map_err(|e: Self::Error| e.set_context(self))
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

impl<'a, 'de> serde::de::MapAccess<'de> for MapAccess<'a, 'de> {
    type Error = Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
    where
        K: serde::de::DeserializeSeed<'de>,
    {
        if self.de.buf.starts_with(b"e") {
            self.de.buf = self.de.buf.get(1..).unwrap_or_default();
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
        let buf_before = self.de.buf;
        let value = seed.deserialize(&mut *self.de)?;
        if self.de.is_torrent_info && self.de.field_context.as_slice() == [ByteBuf(b"info")] {
            let len = self.de.buf.as_ptr() as usize - buf_before.as_ptr() as usize;
            let mut hash = Sha1::new();
            hash.update(&buf_before[..len]);
            let digest = hash.finish();
            self.de.torrent_info_digest = Some(digest)
        }
        self.de.field_context.pop();
        Ok(value)
    }
}

impl<'a, 'de> serde::de::SeqAccess<'de> for SeqAccess<'a, 'de> {
    type Error = Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: serde::de::DeserializeSeed<'de>,
    {
        if self.de.buf.starts_with(b"e") {
            self.de.buf = self.de.buf.get(1..).unwrap_or_default();
            return Ok(None);
        }
        Ok(Some(seed.deserialize(&mut *self.de)?))
    }
}
