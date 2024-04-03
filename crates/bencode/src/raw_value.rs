use super::*;
use serde::de::value::SeqAccessDeserializer;
use serde::de::SeqAccess;

#[derive(Debug)]
pub struct RawValue<T> {
    bytes: T,
}

impl<T> RawValue<T> {
    pub fn new(value: T) -> Self {
        value.into()
    }
}

impl<T, U> PartialEq<RawValue<U>> for RawValue<T>
where
    T: PartialEq<U>,
{
    fn eq(&self, other: &RawValue<U>) -> bool {
        self.bytes.eq(&other.bytes)
    }
}

impl<T> Eq for RawValue<T> where T: Eq {}

impl<T: Clone> Clone for RawValue<T> {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
        }
    }
}

impl<T: CloneToOwned> CloneToOwned for RawValue<T> {
    type Target = RawValue<<T as CloneToOwned>::Target>;

    fn clone_to_owned(&self) -> Self::Target {
        // Why can't I use Self::Target here?
        RawValue {
            bytes: self.bytes.clone_to_owned(),
        }
    }
}
// This can't go in RawValue because it doesn't depend on T.
pub(crate) const TOKEN: &str = "$librqbit_bencode::private::RawValue";

impl<T> Serialize for RawValue<T>
where
    T: AsRef<[u8]>,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_newtype_struct(TOKEN, serde_bytes::Bytes::new(self.bytes.as_ref()))
    }
}

impl<T> From<T> for RawValue<T> {
    fn from(value: T) -> Self {
        Self { bytes: value }
    }
}

impl<'de, T> Deserialize<'de> for RawValue<T>
where
    // Using T: Deserialize instead of From<&[u8]> avoids lifetime issues with 'de. It does mean we use
    // the BorrowedBytesDeserializer to get the bytes into T.
    T: serde::Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct V<T> {
            phantom: PhantomData<T>,
        }
        impl<'de, T: serde::Deserialize<'de>> Visitor<'de> for V<T> {
            type Value = T;

            fn expecting(&self, f: &mut Formatter) -> std::fmt::Result {
                f.write_str("borrowed bytes")
            }

            fn visit_bytes<E>(self, _v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                todo!()
            }

            fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                // This calls Deserialize for the inner type, which hopefully supports &[u8]?
                T::deserialize(BorrowedBytesDeserializer::new(v))
            }

            fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: Deserializer<'de>,
            {
                deserializer.deserialize_bytes(self)
            }

            fn visit_seq<A>(self, seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                <Self::Value as serde::Deserialize>::deserialize(SeqAccessDeserializer::new(seq))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let (key, value): (&str, T) = map
                    .next_entry()?
                    .ok_or(A::Error::invalid_length(0, &"token field"))?;
                if key != TOKEN {
                    return Err(A::Error::unknown_field(key, &[TOKEN]));
                }
                if let Some(key) = map.next_key()? {
                    return Err(A::Error::unknown_field(key, &[TOKEN]));
                }
                Ok(value)
            }
        }
        let visitor: V<T> = V {
            phantom: Default::default(),
        };
        deserializer
            .deserialize_newtype_struct(TOKEN, visitor)
            .map(Into::into)
    }
}

impl<T> std::ops::Deref for RawValue<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

pub(crate) struct RawValueSerializer<'ser, W: std::io::Write> {
    pub(crate) ser: &'ser mut BencodeSerializer<W>,
}

impl<'ser, W: std::io::Write> serde::ser::SerializeStruct for RawValueSerializer<'ser, W> {
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
        value.serialize(RawValueSerializer { ser: self.ser })
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(())
    }
}

impl<'ser, W: std::io::Write> RawValueSerializer<'ser, W> {
    fn expected_err<T>() -> Result<T, SerError> {
        todo!()
        // Err(SerError::custom("expected RawValue"))
    }
}

impl<'ser, W: std::io::Write> serde::ser::SerializeSeq for RawValueSerializer<'ser, W> {
    type Ok = ();
    type Error = SerError;

    fn serialize_element<T: ?Sized>(&mut self, value: &T) -> Result<Self::Ok, Self::Error>
    where
        T: Serialize,
    {
        value.serialize(RawValueSerializer { ser: self.ser })
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(())
    }
}

impl<'ser, W: std::io::Write> Serializer for RawValueSerializer<'ser, W> {
    type Ok = ();
    type Error = SerError;
    type SerializeSeq = RawValueSerializer<'ser, W>;
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

    fn serialize_u8(self, v: u8) -> Result<Self::Ok, Self::Error> {
        self.ser.write_raw(&[v])
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
        self.ser.write_raw(v)
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
        Ok(self)
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
                spam: RawValue::new(b"4:eggs"[..].into())
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
        let wrapper: RawValue<&[u8]> = from_bytes(input).unwrap();
        assert_eq!(wrapper, input.into());

        let buf = to_bytes(&wrapper).unwrap();
        assert_eq!(
            input,
            buf.as_slice(),
            "{} {}",
            input.escape_ascii(),
            buf.escape_ascii()
        )
    }

    #[derive(Serialize)]
    struct Info<Buf> {
        files: Vec<Buf>,
    }
    #[derive(Serialize, PartialEq, Deserialize, Debug)]
    struct MetainfoLike<Buf: Serialize + AsRef<[u8]>> {
        comment: String,
        info: Option<RawValue<Buf>>,
    }

    #[test]
    fn test_to_json_and_back() -> anyhow::Result<()> {
        let orig_info = Info {
            files: vec!["awesome movie".to_string()],
        };
        let orig_meta = MetainfoLike {
            comment: "leet ripper".to_string(),
            info: Some(to_bytes(orig_info)?.into()),
        };
        let json = serde_json::to_string(&orig_meta)?;
        dbg!(&json);
        // Need to allocate on deserialization from JSON array
        let json_meta: MetainfoLike<Vec<u8>> = serde_json::from_str(&json)?;
        assert_eq!(orig_meta, json_meta);
        Ok(())
    }

    #[test]
    fn test_serialize_raw_info_and_back() -> anyhow::Result<()> {
        let orig_info = Info {
            files: vec![ByteString(b"awesome movie"[..].to_owned())],
        };
        let orig_meta = MetainfoLike {
            comment: "leet ripper".to_string(),
            info: Some(RawValue::new(ByteString(to_bytes(orig_info)?))),
        };
        let bytes = to_bytes(&orig_meta)?;
        dbg!(&bytes);
        let json_meta = from_bytes(&bytes)?;
        assert_eq!(orig_meta, json_meta);
        Ok(())
    }
}
