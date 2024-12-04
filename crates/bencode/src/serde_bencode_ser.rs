use std::collections::BTreeMap;

use serde::{ser::Impossible, Serialize, Serializer};

use buffers::ByteBufOwned;

#[derive(Debug)]
pub enum SerErrorKind {
    Other(anyhow::Error),
}

impl std::fmt::Display for SerErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SerErrorKind::Other(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Debug)]
pub struct SerError {
    kind: SerErrorKind,
}

impl SerError {
    fn custom_with_ser<T: std::fmt::Display, W: std::io::Write>(
        msg: T,
        _ser: &BencodeSerializer<W>,
    ) -> Self {
        serde::ser::Error::custom(msg)
    }
    fn from_err_with_ser<E: std::error::Error + Send + Sync + 'static, W: std::io::Write>(
        err: E,
        _ser: &BencodeSerializer<W>,
    ) -> Self {
        Self {
            kind: SerErrorKind::Other(err.into()),
        }
    }
}

impl serde::ser::Error for SerError {
    fn custom<T>(msg: T) -> Self
    where
        T: std::fmt::Display,
    {
        Self {
            kind: SerErrorKind::Other(anyhow::anyhow!("{}", msg)),
        }
    }
}

impl std::error::Error for SerError {}

impl std::fmt::Display for SerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.kind)
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
    fn write_raw(&mut self, buf: &[u8]) -> Result<(), SerError> {
        self.writer
            .write_all(buf)
            .map_err(|e| SerError::from_err_with_ser(e, self))
    }
    fn write_fmt(&mut self, fmt: core::fmt::Arguments<'_>) -> Result<(), SerError> {
        self.writer
            .write_fmt(fmt)
            .map_err(|e| SerError::from_err_with_ser(e, self))
    }
    fn write_byte(&mut self, byte: u8) -> Result<(), SerError> {
        self.write_raw(&[byte])
    }
    fn write_number<N: std::fmt::Display>(&mut self, number: N) -> Result<(), SerError> {
        self.write_fmt(format_args!("i{number}e"))
    }
    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), SerError> {
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

    type Error = SerError;

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

    type Error = SerError;

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

    type Error = SerError;

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
        // key.serialize(&mut *self.ser);
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
            self.ser.write_bytes(&key)?;
            self.ser.write_raw(&value)?;
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

    type Error = SerError;

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
            self.ser.write_raw(&value)?;
        }
        self.ser.write_byte(b'e')
    }
}

impl<'ser, W: std::io::Write> Serializer for &'ser mut BencodeSerializer<W> {
    type Ok = ();
    type Error = SerError;
    type SerializeSeq = SerializeSeq<'ser, W>;
    type SerializeTuple = SerializeTuple<'ser, W>;
    type SerializeTupleStruct = Impossible<(), SerError>;
    type SerializeTupleVariant = Impossible<(), SerError>;
    type SerializeMap = SerializeMap<'ser, W>;
    type SerializeStruct = SerializeStruct<'ser, W>;
    type SerializeStructVariant = Impossible<(), SerError>;

    fn serialize_bool(self, _: bool) -> Result<Self::Ok, Self::Error> {
        Err(SerError::custom_with_ser(
            "bencode doesn't support booleans",
            self,
        ))
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
        Err(SerError::custom_with_ser(
            "bencode doesn't support f32",
            self,
        ))
    }

    fn serialize_f64(self, _: f64) -> Result<Self::Ok, Self::Error> {
        Err(SerError::custom_with_ser(
            "bencode doesn't support f32",
            self,
        ))
    }

    fn serialize_char(self, _: char) -> Result<Self::Ok, Self::Error> {
        Err(SerError::custom_with_ser(
            "bencode doesn't support chars",
            self,
        ))
    }

    fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
        self.write_bytes(v.as_bytes())
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok, Self::Error> {
        self.write_bytes(v)
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        Err(SerError::custom_with_ser(
            "bencode doesn't support None",
            self,
        ))
    }

    fn serialize_some<T>(self, value: &T) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + serde::Serialize,
    {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Err(SerError::custom_with_ser(
            "bencode doesn't support Rust unit ()",
            self,
        ))
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        Err(SerError::custom_with_ser(
            "bencode doesn't support unit structs",
            self,
        ))
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        Err(SerError::custom_with_ser(
            "bencode doesn't support unit variants",
            self,
        ))
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
        Err(SerError::custom_with_ser(
            "bencode doesn't support newtype structs",
            self,
        ))
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
        Err(SerError::custom_with_ser(
            "bencode doesn't support newtype variants",
            self,
        ))
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        self.write_byte(b'l')?;
        Ok(SerializeSeq { ser: self })
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Err(SerError::custom_with_ser(
            "bencode doesn't support tuples",
            self,
        ))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Err(SerError::custom_with_ser(
            "bencode doesn't support tuple structs",
            self,
        ))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Err(SerError::custom_with_ser(
            "bencode doesn't support tuple variants",
            self,
        ))
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
        Err(SerError::custom_with_ser(
            "bencode doesn't support struct variants",
            self,
        ))
    }
}

pub fn bencode_serialize_to_writer<T: Serialize, W: std::io::Write>(
    value: T,
    writer: &mut W,
) -> Result<(), SerError> {
    let mut serializer = BencodeSerializer::new(writer);
    value.serialize(&mut serializer)?;
    Ok(())
}
