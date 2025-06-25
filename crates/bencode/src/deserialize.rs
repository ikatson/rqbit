use std::marker::PhantomData;

use arrayvec::ArrayVec;
use atoi::FromRadix10;
use buffers::ByteBuf;
use clone_to_owned::CloneToOwned;
use serde::{Deserialize, Serialize, forward_to_deserialize_any};

use crate::raw_value::TAG;

pub struct BencodeDeserializer<'de> {
    buf: &'de [u8],
    field_context: ErrorContext<'de>,
    field_context_did_not_fit: u8,
    parsing_key: bool,
}

impl<'de> BencodeDeserializer<'de> {
    pub fn new_from_buf(buf: &'de [u8]) -> BencodeDeserializer<'de> {
        Self {
            buf,
            field_context: Default::default(),
            field_context_did_not_fit: 0,
            parsing_key: false,
        }
    }

    pub fn into_remaining(self) -> &'de [u8] {
        self.buf
    }

    #[inline]
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

    fn parse_integer<I: FromRadix10>(&mut self) -> Result<I, Error> {
        self.parse_first_byte(b'i', Error::InvalidValue)?;
        match I::from_radix_10(self.buf) {
            (v, len) if len > 0 && self.buf.get(len) == Some(&b'e') => {
                self.buf = &self.buf[len + 1..];
                Ok(v)
            }
            _ => Err(Error::InvalidValue),
        }
    }

    fn parse_bytes(&mut self) -> Result<&'de [u8], Error> {
        let b = match usize::from_radix_10(self.buf) {
            (v, len) if len > 0 && self.buf.get(len) == Some(&b':') => {
                self.buf = unsafe { self.buf.get_unchecked(len + 1..) };
                let (bytes, rest) = self.buf.split_at_checked(v).ok_or(Error::Eof)?;
                self.buf = rest;
                bytes
            }
            _ => return Err(Error::InvalidValue),
        };
        if self.parsing_key && self.field_context.try_push(ByteBuf(b)).is_err() {
            self.field_context_did_not_fit = self.field_context_did_not_fit.saturating_add(1);
        }
        Ok(b)
    }
}

/// Deserialize a bencode value. If there are trailing bytes, will error.
pub fn from_bytes<'a, T>(buf: &'a [u8]) -> Result<T, ErrorWithContext<'a>>
where
    T: serde::de::Deserialize<'a>,
{
    let (v, rest) = from_bytes_with_rest(buf)?;
    if !rest.is_empty() {
        return Err(ErrorWithContext {
            kind: Error::BytesRemaining(rest.len()),
            ctx: Default::default(),
        });
    }
    Ok(v)
}

/// Deserialize a bencode value at the start of the buffer, return it and the remaining bytes.
pub fn from_bytes_with_rest<'a, T>(buf: &'a [u8]) -> Result<(T, &'a [u8]), ErrorWithContext<'a>>
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
    Ok((v, de.buf))
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("not supported by bencode")]
    NotSupported,
    #[error("{0}")]
    Custom(Box<String>), // box to reduce size
    #[error("expected 0 or 1 for boolean, got {0}")]
    InvalidBool(u8),
    #[error("deserialized successfully, but {0} bytes remaining")]
    BytesRemaining(usize),
    #[error("invalid length: {0}")]
    InvalidLength(usize),
    #[error("invalid value")]
    InvalidValue,
    #[error("WithRawValue: invalid value")]
    RawDeInvalidValue,
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
            Some(b'i') => self.deserialize_i64(visitor),
            Some(b'l') => self.deserialize_seq(visitor),
            Some(_) => self.deserialize_bytes(visitor),
            None => Err(Error::Eof),
        }
    }

    fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        let value = self.parse_integer::<u8>()?;
        if value > 1 {
            return Err(Error::InvalidBool(value));
        }
        visitor.visit_bool(value == 1)
    }

    fn deserialize_i8<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_i8(self.parse_integer()?)
    }

    fn deserialize_i16<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_i16(self.parse_integer()?)
    }

    fn deserialize_i32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_i32(self.parse_integer()?)
    }

    fn deserialize_i64<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_i64(self.parse_integer()?)
    }

    fn deserialize_u8<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_u8(self.parse_integer()?)
    }

    fn deserialize_u16<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_u16(self.parse_integer()?)
    }

    fn deserialize_u32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_u32(self.parse_integer()?)
    }

    fn deserialize_u64<V>(self, visitor: V) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        visitor.visit_u64(self.parse_integer()?)
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
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error>
    where
        V: serde::de::Visitor<'de>,
    {
        if name == TAG {
            return visitor.visit_seq(WithRawValueDeserializer {
                de: self,
                buf: None,
            });
        }
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
        let value = seed.deserialize(&mut *self.de)?;
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

struct WithRawValueDeserializer<'a, 'de> {
    de: &'a mut BencodeDeserializer<'de>,
    buf: Option<&'de [u8]>,
}

impl<'a, 'de> serde::de::SeqAccess<'de> for WithRawValueDeserializer<'a, 'de> {
    type Error = Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
    where
        T: serde::de::DeserializeSeed<'de>,
    {
        let buf = match self.buf {
            None => {
                let buf_before = self.de.buf;
                let el = seed.deserialize(&mut *self.de)?;
                let buf_after = self.de.buf;
                let consumed = buf_before.len() - buf_after.len();
                self.buf = Some(&buf_before[..consumed]);
                return Ok(Some(el));
            }
            Some(buf) => buf,
        };

        struct RawValueDe<'a>(&'a [u8]);

        impl<'de> serde::de::Deserializer<'de> for RawValueDe<'de> {
            type Error = Error;

            fn deserialize_any<V>(self, _: V) -> Result<V::Value, Self::Error>
            where
                V: serde::de::Visitor<'de>,
            {
                Err(Error::RawDeInvalidValue)
            }

            fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value, Self::Error>
            where
                V: serde::de::Visitor<'de>,
            {
                visitor.visit_borrowed_bytes(self.0)
            }

            fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value, Self::Error>
            where
                V: serde::de::Visitor<'de>,
            {
                visitor.visit_borrowed_bytes(self.0)
            }

            forward_to_deserialize_any! {
                bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
                option unit unit_struct newtype_struct seq tuple
                tuple_struct map struct enum identifier ignored_any
            }
        }

        let buf = seed.deserialize(RawValueDe(buf))?;
        Ok(Some(buf))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithRawBytes<T, Buf> {
    pub data: T,
    pub raw_bytes: Buf,
}

impl<T: CloneToOwned, Buf: CloneToOwned> CloneToOwned for WithRawBytes<T, Buf> {
    type Target = WithRawBytes<T::Target, Buf::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&bytes::Bytes>) -> Self::Target {
        WithRawBytes {
            data: self.data.clone_to_owned(within_buffer),
            raw_bytes: self.raw_bytes.clone_to_owned(within_buffer),
        }
    }
}

impl<T: Serialize, Buf> Serialize for WithRawBytes<T, Buf> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.data.serialize(serializer)
    }
}

impl<'de, T, Buf> Deserialize<'de> for WithRawBytes<T, Buf>
where
    T: Deserialize<'de>,
    Buf: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor<T, Buf>(PhantomData<(T, Buf)>);
        impl<'de, T, Buf> serde::de::Visitor<'de> for Visitor<T, Buf>
        where
            T: Deserialize<'de>,
            Buf: Deserialize<'de>,
        {
            type Value = WithRawBytes<T, Buf>;

            fn expecting(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
                fmt.write_str("WithRawBytes only works with librqbit_bencode")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let data: T = match seq.next_element()? {
                    Some(v) => v,
                    None => {
                        return Err(<A::Error as serde::de::Error>::custom(
                            "expecting T as first element",
                        ));
                    }
                };
                let raw_bytes: Buf = match seq.next_element()? {
                    Some(b) => b,
                    None => {
                        return Err(<A::Error as serde::de::Error>::custom(
                            "expecting buf as second element",
                        ));
                    }
                };
                Ok(WithRawBytes { data, raw_bytes })
            }
        }
        deserializer.deserialize_newtype_struct(TAG, Visitor(Default::default()))
    }
}

#[cfg(test)]
mod tests {
    use buffers::ByteBuf;
    use serde::Deserialize;

    use crate::{WithRawBytes, from_bytes};

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
        assert_eq!(format!("{e}"), "\"child\" -> \"key\": invalid value");
    }

    #[test]
    fn test_int() {
        assert_eq!(from_bytes::<u8>(b"i42e").unwrap(), 42);
        assert_eq!(from_bytes::<u16>(b"i42e").unwrap(), 42);
        assert_eq!(from_bytes::<u32>(b"i42e").unwrap(), 42);
        assert_eq!(from_bytes::<u64>(b"i42e").unwrap(), 42);

        assert_eq!(from_bytes::<u32>(b"i4294967295e").unwrap(), 4294967295);

        assert!(from_bytes::<u32>(b"ie").is_err());
        assert!(from_bytes::<u32>(b"ifooe").is_err());
        assert!(from_bytes::<u32>(b"i42").is_err());

        // trailing bytes
        assert!(from_bytes::<u32>(b"i42ee").is_err());
    }

    #[test]
    fn test_str() {
        assert_eq!(
            from_bytes::<ByteBuf<'_>>(b"5:hello").unwrap(),
            ByteBuf(b"hello")
        );
        assert_eq!(from_bytes::<ByteBuf<'_>>(b"0:").unwrap(), ByteBuf(b""));

        assert!(from_bytes::<ByteBuf<'_>>(b"5:hell").is_err());
        assert!(from_bytes::<ByteBuf<'_>>(b"5:helloworld").is_err());
    }

    #[test]
    fn test_struct() {
        #[derive(Deserialize, Eq, PartialEq, Debug)]
        struct S<'a> {
            key: u32,
            #[serde(borrow)]
            value: Option<ByteBuf<'a>>,
        }

        assert_eq!(
            from_bytes::<S<'_>>(b"d3:keyi42ee").unwrap(),
            S {
                key: 42,
                value: None
            }
        );

        assert_eq!(
            from_bytes::<S<'_>>(b"d3:keyi42e5:value5:helloe").unwrap(),
            S {
                key: 42,
                value: Some(ByteBuf(b"hello"))
            }
        );
    }

    #[test]
    fn test_list() {
        assert_eq!(from_bytes::<Vec<ByteBuf<'_>>>(b"le").unwrap(), vec![]);
        assert_eq!(
            from_bytes::<Vec<ByteBuf<'_>>>(b"l5:hello2:mee").unwrap(),
            vec![ByteBuf(b"hello"), ByteBuf(b"me")]
        );
    }

    #[test]
    fn test_with_raw_bytes() {
        #[derive(Deserialize, Debug)]
        struct TorrentInfo<'a> {
            #[serde(borrow)]
            name: ByteBuf<'a>,
        }

        #[derive(Deserialize, Debug)]
        struct Torrent<'a> {
            #[serde(borrow)]
            info: WithRawBytes<TorrentInfo<'a>, ByteBuf<'a>>,
        }

        let t: Torrent = from_bytes(&b"d4:infod4:name5:helloee"[..]).unwrap();
        assert_eq!(t.info.data.name, ByteBuf(b"hello"));
        assert_eq!(t.info.raw_bytes, ByteBuf(b"d4:name5:helloe"));
    }

    #[test]
    fn test_dict() {
        use std::collections::BTreeMap;
        let mut m = BTreeMap::new();
        m.insert(ByteBuf(b"key"), ByteBuf(b"value"));
        m.insert(ByteBuf(b"key2"), ByteBuf(b"value2"));
        assert_eq!(
            from_bytes::<BTreeMap<ByteBuf, ByteBuf>>(b"d3:key5:value4:key26:value2e").unwrap(),
            m
        );
    }

    #[test]
    fn test_struct_unknown_field() {
        #[derive(Deserialize, Eq, PartialEq, Debug)]
        struct S {
            key: u32,
        }

        assert_eq!(
            from_bytes::<S>(b"d3:keyi42e5:value5:helloe").unwrap(),
            S { key: 42 }
        );
    }

    #[test]
    fn test_complex_struct() {
        #[derive(Deserialize, Eq, PartialEq, Debug)]
        struct S<'a> {
            key: u32,
            #[serde(borrow)]
            values: Vec<ByteBuf<'a>>,
        }

        assert_eq!(
            from_bytes::<S>(b"d3:keyi42e6:valuesl5:hello5:worldee").unwrap(),
            S {
                key: 42,
                values: vec![ByteBuf(b"hello"), ByteBuf(b"world")]
            }
        );
    }
}
