use super::*;
use serde::ser::Error;

#[derive(Debug)]
pub struct RawValue<T>(pub T);

impl<T> PartialEq<Self> for RawValue<T>
where
    T: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.0.eq(other)
    }
}

impl<T> Eq for RawValue<T> where T: Eq {}

impl<T: Clone> Clone for RawValue<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

// This can't go in RawValue because it doesn't depend on T.
pub(crate) const TOKEN: &str = "$librqbit_bencode::private::RawValue";

impl<T> Serialize for RawValue<T>
where
    T: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut s = serializer.serialize_struct(TOKEN, 1)?;
        s.serialize_field(TOKEN, &self.0)?;
        s.end()
    }
}

impl<'de, T> Deserialize<'de> for RawValue<T>
where
    T: From<&'de [u8]>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = &'de [u8];

            fn expecting(&self, _formatter: &mut Formatter) -> std::fmt::Result {
                todo!()
            }

            fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(v)
            }
        }
        deserializer
            .deserialize_newtype_struct(TOKEN, V)
            .map(|x| RawValue(x.into()))
    }
}

impl<T> std::ops::Deref for RawValue<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub(crate) struct SerializeRawValue<'ser, W: std::io::Write> {
    pub(crate) ser: &'ser mut BencodeSerializer<W>,
}

impl<'ser, W: std::io::Write> serde::ser::SerializeStruct for SerializeRawValue<'ser, W> {
    type Ok = ();
    type Error = SerError;

    fn serialize_field<T: ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Self::Error>
    where
        T: Serialize,
    {
        assert_eq!(key, TOKEN);
        value.serialize(RawValueSerializer(self.ser))
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(())
    }
}

pub(crate) struct RawValueSerializer<'ser, W: std::io::Write>(&'ser mut BencodeSerializer<W>);

impl<'ser, W: std::io::Write> RawValueSerializer<'ser, W> {
    fn expected_err<T>() -> Result<T, SerError> {
        Err(SerError::custom("expected RawValue"))
    }
}

impl<'ser, W: std::io::Write> Serializer for RawValueSerializer<'ser, W> {
    type Ok = ();
    type Error = SerError;
    type SerializeSeq = Impossible<(), SerError>;
    type SerializeTuple = Impossible<(), SerError>;
    type SerializeTupleStruct = Impossible<(), SerError>;
    type SerializeTupleVariant = Impossible<(), SerError>;
    type SerializeMap = Impossible<(), SerError>;
    type SerializeStruct = Impossible<(), SerError>;
    type SerializeStructVariant = Impossible<(), SerError>;

    fn serialize_bool(self, _v: bool) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_i8(self, _v: i8) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_i16(self, _v: i16) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_i32(self, _v: i32) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_i64(self, _v: i64) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_u8(self, _v: u8) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_u16(self, _v: u16) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_u32(self, _v: u32) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_u64(self, _v: u64) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_f32(self, _v: f32) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_f64(self, _v: f64) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_char(self, _v: char) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_str(self, _v: &str) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok, Self::Error> {
        self.0.write_raw(v)
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_some<T: ?Sized>(self, _value: &T) -> Result<Self::Ok, Self::Error>
    where
        T: Serialize,
    {
        Self::expected_err()
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        Self::expected_err()
    }

    fn serialize_newtype_struct<T: ?Sized>(
        self,
        _name: &'static str,
        _value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: Serialize,
    {
        Self::expected_err()
    }

    fn serialize_newtype_variant<T: ?Sized>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: Serialize,
    {
        Self::expected_err()
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Self::expected_err()
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Self::expected_err()
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Self::expected_err()
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Self::expected_err()
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Self::expected_err()
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Self::expected_err()
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Self::expected_err()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_value_field() {
        #[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
        struct Object {
            cow: String,
            spam: RawValue<ByteString>,
        }

        let input = b"d3:cow3:moo4:spam4:eggse";
        let object: Object = from_bytes(input).unwrap();
        assert_eq!(
            object,
            Object {
                cow: "moo".to_owned(),
                spam: RawValue(b"4:eggs"[..].into())
            }
        );

        let buf = to_bytes(&object).unwrap();
        assert_eq!(input, buf.as_slice())
    }

    #[test]
    fn test_entire_value() {
        #[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
        struct Object {
            cow: String,
            spam: String,
        }

        let input = &b"d3:cow3:moo4:spam4:eggse"[..];
        let wrapper: RawValue<ByteBuf> = from_bytes(input).unwrap();
        assert_eq!(wrapper, RawValue(input.into()));

        let buf = to_bytes(&wrapper).unwrap();
        assert_eq!(input, buf.as_slice())
    }
}
