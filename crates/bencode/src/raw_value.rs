use std::marker::PhantomData;

use buffers::ByteBufT;
use serde::{Deserialize, Serialize, Serializer};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WithRawBytes<T, BufT> {
    pub object: T,
    pub raw_bytes: BufT,
}

pub const TOKEN: &str = "$librqbit_bencode::RawValue";

impl<T, BufT> Serialize for WithRawBytes<T, BufT>
where
    T: Serialize,
    BufT: ByteBufT,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        struct SerializeBytes<'a>(&'a [u8]);

        impl<'a> Serialize for SerializeBytes<'a> {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_bytes(self.0)
            }
        }

        // Serializer MUST respect the TOKEN and just write the bytes as they are to the output.
        serializer.serialize_newtype_struct(TOKEN, &SerializeBytes(self.raw_bytes.as_slice()))
    }
}

impl<'de, T, BufT> Deserialize<'de> for WithRawBytes<T, BufT>
where
    T: Deserialize<'de>,
    BufT: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor<T, BufT> {
            phantom: PhantomData<(T, BufT)>,
        }

        impl<'de, T, BufT> serde::de::Visitor<'de> for Visitor<T, BufT>
        where
            T: Deserialize<'de>,
            BufT: Deserialize<'de>,
        {
            type Value = WithRawBytes<T, BufT>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(
                    formatter,
                    "deserializer to call visit_map() with 2 fields: object and bytes"
                )
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                use serde::de::Error;
                let object = match seq.next_element::<T>()? {
                    Some(object) => object,
                    _ => return Err(A::Error::custom("expected to be able to decode object")),
                };
                let raw_bytes = match seq.next_element::<BufT>()? {
                    Some(object) => object,
                    None => return Err(A::Error::custom("expected to be able to decode bytes")),
                };

                Ok(WithRawBytes { object, raw_bytes })
            }
        }

        // Serializer MUST respect the TOKEN, and deserialize it as a map with exactly 2 items.
        //

        deserializer.deserialize_struct(
            TOKEN,
            &[],
            Visitor {
                phantom: Default::default(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use buffers::ByteString;
    use serde::{Deserialize, Serialize};

    use crate::{bencode_serialize_to_writer, from_bytes, raw_value::WithRawBytes};

    #[test]
    fn test_with_raw_bytes_1() {
        #[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
        struct Object {
            cow: String,
            spam: WithRawBytes<String, ByteString>,
        }

        let input = b"d3:cow3:moo4:spam4:eggse";
        let object: Object = from_bytes(input).unwrap();
        assert_eq!(
            object,
            Object {
                cow: "moo".to_owned(),
                spam: WithRawBytes {
                    object: "eggs".to_owned(),
                    raw_bytes: b"4:eggs"[..].into()
                }
            }
        );

        let mut buf = Vec::new();
        bencode_serialize_to_writer(&object, &mut buf).unwrap();
        assert_eq!(input, buf.as_slice())
    }

    #[test]
    fn test_with_raw_bytes_2() {
        #[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
        struct Object {
            cow: String,
            spam: String,
        }

        type Wrapper = WithRawBytes<Object, ByteString>;

        let input = &b"d3:cow3:moo4:spam4:eggse"[..];
        let wrapper: Wrapper = from_bytes(input).unwrap();
        assert_eq!(
            wrapper,
            Wrapper {
                object: Object {
                    cow: "moo".to_owned(),
                    spam: "eggs".to_owned()
                },
                raw_bytes: input.into()
            }
        );

        let mut buf = Vec::new();
        bencode_serialize_to_writer(&wrapper, &mut buf).unwrap();
        assert_eq!(input, buf.as_slice())
    }
}
