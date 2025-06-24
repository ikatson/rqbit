use std::collections::BTreeMap;

use serde::{Serialize, Serializer, ser::Impossible};

use buffers::ByteBufOwned;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Io(std::io::Error),
    #[error("{0}")]
    Custom(Box<Box<str>>), // double box to reduce size
    #[error("{0}")]
    Text(&'static &'static str),
}

impl serde::ser::Error for Error {
    fn custom<T>(msg: T) -> Self
    where
        T: std::fmt::Display,
    {
        Error::Custom(Box::new(msg.to_string().into_boxed_str()))
    }
}

struct BencodeSerializer<W: std::io::Write> {
    writer: W,
    hack_no_bytestring_prefix: bool,
}

impl<W: std::io::Write> BencodeSerializer<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            hack_no_bytestring_prefix: false,
        }
    }
    fn write_raw(&mut self, buf: &[u8]) -> Result<(), Error> {
        self.writer.write_all(buf).map_err(Error::Io)
    }
    fn write_fmt(&mut self, fmt: core::fmt::Arguments<'_>) -> Result<(), Error> {
        self.writer.write_fmt(fmt).map_err(Error::Io)
    }
    fn write_byte(&mut self, byte: u8) -> Result<(), Error> {
        self.write_raw(&[byte])
    }
    fn write_number<N: std::fmt::Display>(&mut self, number: N) -> Result<(), Error> {
        self.write_fmt(format_args!("i{number}e"))
    }
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if !self.hack_no_bytestring_prefix {
            self.write_fmt(format_args!("{}:", bytes.len()))?;
        }
        self.write_raw(bytes)
    }
}

struct SerializeSeq<'ser, W: std::io::Write> {
    ser: &'ser mut BencodeSerializer<W>,
}
impl<W: std::io::Write> serde::ser::SerializeSeq for SerializeSeq<'_, W> {
    type Ok = ();

    type Error = Error;

    fn serialize_element<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + serde::Serialize,
    {
        value.serialize(&mut *self.ser)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.ser.write_byte(b'e')
    }
}

struct SerializeTuple<'ser, W: std::io::Write> {
    ser: &'ser mut BencodeSerializer<W>,
}
impl<W: std::io::Write> serde::ser::SerializeTuple for SerializeTuple<'_, W> {
    type Ok = ();

    type Error = Error;

    fn serialize_element<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + serde::Serialize,
    {
        value.serialize(&mut *self.ser)
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.ser.write_byte(b'e')
    }
}

struct SerializeMap<'ser, W: std::io::Write> {
    ser: &'ser mut BencodeSerializer<W>,
    tmp: BTreeMap<ByteBufOwned, ByteBufOwned>,
    last_key: Option<ByteBufOwned>,
}
impl<W: std::io::Write> serde::ser::SerializeMap for SerializeMap<'_, W> {
    type Ok = ();

    type Error = Error;

    fn serialize_key<T>(&mut self, key: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + serde::Serialize,
    {
        let mut buf = Vec::new();
        let mut ser = BencodeSerializer::new(&mut buf);
        ser.hack_no_bytestring_prefix = true;
        key.serialize(&mut ser)?;
        self.last_key.replace(ByteBufOwned::from(buf));
        Ok(())
    }

    fn serialize_value<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + serde::Serialize,
    {
        let mut buf = Vec::new();
        let mut ser = BencodeSerializer::new(&mut buf);
        value.serialize(&mut ser)?;
        self.tmp
            .insert(self.last_key.take().unwrap(), ByteBufOwned::from(buf));
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        for (key, value) in self.tmp {
            self.ser.write_bytes(key.as_ref())?;
            self.ser.write_raw(value.as_ref())?;
        }
        self.ser.write_byte(b'e')
    }
}

struct SerializeStruct<'ser, W: std::io::Write> {
    ser: &'ser mut BencodeSerializer<W>,
    tmp: BTreeMap<&'static str, ByteBufOwned>,
}
impl<W: std::io::Write> serde::ser::SerializeStruct for SerializeStruct<'_, W> {
    type Ok = ();

    type Error = Error;

    fn serialize_field<T>(&mut self, key: &'static str, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + serde::Serialize,
    {
        let mut buf = Vec::new();
        let mut ser = BencodeSerializer::new(&mut buf);
        value.serialize(&mut ser)?;
        self.tmp.insert(key, ByteBufOwned::from(buf));
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        for (key, value) in self.tmp {
            self.ser.write_bytes(key.as_bytes())?;
            self.ser.write_raw(value.as_ref())?;
        }
        self.ser.write_byte(b'e')
    }
}

impl<'ser, W: std::io::Write> Serializer for &'ser mut BencodeSerializer<W> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = SerializeSeq<'ser, W>;
    type SerializeTuple = SerializeTuple<'ser, W>;
    type SerializeTupleStruct = Impossible<(), Error>;
    type SerializeTupleVariant = Impossible<(), Error>;
    type SerializeMap = SerializeMap<'ser, W>;
    type SerializeStruct = SerializeStruct<'ser, W>;
    type SerializeStructVariant = Impossible<(), Error>;

    fn serialize_bool(self, value: bool) -> Result<Self::Ok, Self::Error> {
        self.write_number(if value { 1 } else { 0 })
    }

    fn serialize_i8(self, v: i8) -> Result<Self::Ok, Self::Error> {
        self.write_number(v)
    }

    fn serialize_i16(self, v: i16) -> Result<Self::Ok, Self::Error> {
        self.write_number(v)
    }

    fn serialize_i32(self, v: i32) -> Result<Self::Ok, Self::Error> {
        self.write_number(v)
    }

    fn serialize_i64(self, v: i64) -> Result<Self::Ok, Self::Error> {
        self.write_number(v)
    }

    fn serialize_u8(self, v: u8) -> Result<Self::Ok, Self::Error> {
        self.write_number(v)
    }

    fn serialize_u16(self, v: u16) -> Result<Self::Ok, Self::Error> {
        self.write_number(v)
    }

    fn serialize_u32(self, v: u32) -> Result<Self::Ok, Self::Error> {
        self.write_number(v)
    }

    fn serialize_u64(self, v: u64) -> Result<Self::Ok, Self::Error> {
        self.write_number(v)
    }

    fn serialize_f32(self, _: f32) -> Result<Self::Ok, Self::Error> {
        Err(Error::Text(&"bencode doesn't support f32"))
    }

    fn serialize_f64(self, _: f64) -> Result<Self::Ok, Self::Error> {
        Err(Error::Text(&"bencode doesn't support f32"))
    }

    fn serialize_char(self, _: char) -> Result<Self::Ok, Self::Error> {
        Err(Error::Text(&"bencode doesn't support chars"))
    }

    fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
        self.write_bytes(v.as_bytes())
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok, Self::Error> {
        self.write_bytes(v)
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        Err(Error::Text(&"bencode doesn't support None"))
    }

    fn serialize_some<T>(self, value: &T) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + serde::Serialize,
    {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Err(Error::Text(&"bencode doesn't support Rust unit ()"))
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        Err(Error::Text(&"bencode doesn't support unit structs"))
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        Err(Error::Text(&"bencode doesn't support unit variants"))
    }

    fn serialize_newtype_struct<T>(
        self,
        name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + serde::Serialize,
    {
        if name == crate::raw_value::TAG {
            self.hack_no_bytestring_prefix = true;
            value.serialize(&mut *self)?;
            self.hack_no_bytestring_prefix = false;
            return Ok(());
        }
        Err(Error::Text(&"bencode doesn't support newtype structs"))
    }

    fn serialize_newtype_variant<T>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + serde::Serialize,
    {
        Err(Error::Text(&"bencode doesn't support newtype variants"))
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        self.write_byte(b'l')?;
        Ok(SerializeSeq { ser: self })
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Err(Error::Text(&"bencode doesn't support tuples"))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Err(Error::Text(&"bencode doesn't support tuple structs"))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Err(Error::Text(&"bencode doesn't support tuple variants"))
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        self.write_byte(b'd')?;
        Ok(SerializeMap {
            ser: self,
            tmp: Default::default(),
            last_key: None,
        })
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        self.write_byte(b'd')?;
        Ok(SerializeStruct {
            ser: self,
            tmp: Default::default(),
        })
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Err(Error::Text(&"bencode doesn't support struct variants"))
    }
}

pub fn bencode_serialize_to_writer<T: Serialize, W: std::io::Write>(
    value: T,
    writer: &mut W,
) -> Result<(), Error> {
    let mut serializer = BencodeSerializer::new(writer);
    value.serialize(&mut serializer)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use buffers::{ByteBuf, ByteBufOwned};
    use serde::Serialize;

    use crate::bencode_serialize_to_writer;

    fn ser<T: Serialize>(value: T) -> Result<ByteBufOwned, crate::SerializeError> {
        let mut vec = Vec::new();
        bencode_serialize_to_writer(&value, &mut vec)?;
        Ok(vec.into())
    }

    #[test]
    fn test_ints() {
        assert_eq!(ser(42u16).unwrap(), b"i42e"[..].into());
        assert_eq!(ser(42u32).unwrap(), b"i42e"[..].into());
        assert_eq!(ser(42u64).unwrap(), b"i42e"[..].into());
    }

    #[test]
    fn test_bytes() {
        assert_eq!(ser(ByteBuf(b"abc")).unwrap(), b"3:abc"[..].into());
        assert_eq!(
            ser(ByteBufOwned::from(&b"abc"[..])).unwrap(),
            b"3:abc"[..].into()
        );
    }

    #[test]
    fn test_seq() {
        assert_eq!(
            ser(&[ByteBuf(b"foo"), ByteBuf(b"bar")][..]).unwrap(),
            b"l3:foo3:bare"[..].into()
        );
        assert_eq!(
            ser(vec![ByteBuf(b"foo"), ByteBuf(b"bar")]).unwrap(),
            b"l3:foo3:bare"[..].into()
        );
    }

    #[test]
    fn test_struct() {
        #[derive(Serialize, Debug)]
        struct S<'a> {
            key: u32,
            value: ByteBuf<'a>,
        }
        assert_eq!(
            ser(S {
                key: 42,
                value: b"foo"[..].into()
            })
            .unwrap(),
            b"d3:keyi42e5:value3:fooe"[..].into()
        );
    }
}
