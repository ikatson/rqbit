use std::{collections::HashMap, marker::PhantomData};

use buffers::{ByteBuf, ByteBufT, ByteString};
use clone_to_owned::CloneToOwned;
use serde::Deserializer;

use super::*;

pub fn dyn_from_bytes<'a, BufT>(buf: &'a [u8]) -> anyhow::Result<BencodeValue<BufT>>
where
    BufT: BencodeValueBufConstraint + From<&'a [u8]>,
{
    from_bytes(buf)
}

impl<BufT: serde::Serialize + BencodeValueBufConstraint> serde::Serialize for BencodeValue<BufT> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            BencodeValue::Bytes(b) => b.serialize(serializer),
            BencodeValue::Integer(v) => v.serialize(serializer),
            BencodeValue::List(l) => l.serialize(serializer),
            BencodeValue::Dict(d) => d.serialize(serializer),
        }
    }
}

impl<'de, BufT> serde::de::Deserialize<'de> for BencodeValue<BufT>
where
    BufT: BencodeValueBufConstraint + From<&'de [u8]>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor<BufT> {
            buftype: PhantomData<BufT>,
        }

        impl<'de, BufT> serde::de::Visitor<'de> for Visitor<BufT>
        where
            BufT: BencodeValueBufConstraint + From<&'de [u8]>,
        {
            type Value = BencodeValue<BufT>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(formatter, "a bencode value")
            }

            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(BencodeValue::Integer(v))
            }

            fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(BencodeValue::Bytes(BufT::from(v)))
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut v = Vec::new();
                while let Some(value) = seq.next_element()? {
                    v.push(value);
                }
                Ok(BencodeValue::List(v))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut hmap = HashMap::new();
                while let Some(key) = map.next_key::<&'de [u8]>()? {
                    let value = map.next_value()?;
                    hmap.insert(BufT::from(key), value);
                }
                Ok(BencodeValue::Dict(hmap))
            }
        }

        deserializer.deserialize_any(Visitor {
            buftype: PhantomData,
        })
    }
}

pub trait BencodeValueBufConstraint: std::hash::Hash + Eq + ByteBufT {}

impl<T> BencodeValueBufConstraint for T where T: std::hash::Hash + Eq + ByteBufT {}

/// A dynamic value when we don't know exactly what we are deserializing.
/// Useful for debugging.
#[derive(PartialEq, Eq, Clone)]
pub enum BencodeValue<BufT: BencodeValueBufConstraint> {
    Bytes(BufT),
    Integer(i64),
    List(Vec<BencodeValue<BufT>>),
    Dict(HashMap<BufT, BencodeValue<BufT>>),
}

impl<BufT> BencodeValue<BufT>
where
    BufT: BencodeValueBufConstraint + serde::Serialize,
{
    pub fn to_bytes(&self) -> Result<Vec<u8>, SerError> {
        let mut bytes = vec![];
        bencode_serialize_to_writer(self, &mut bytes)?;
        Ok(bytes)
    }
}

impl<BufT: std::fmt::Debug + BencodeValueBufConstraint> std::fmt::Debug for BencodeValue<BufT> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BencodeValue::Bytes(b) => std::fmt::Debug::fmt(b, f),
            BencodeValue::Integer(i) => std::fmt::Debug::fmt(i, f),
            BencodeValue::List(l) => std::fmt::Debug::fmt(l, f),
            BencodeValue::Dict(d) => std::fmt::Debug::fmt(d, f),
        }
    }
}

impl<BufT> CloneToOwned for BencodeValue<BufT>
where
    BufT: CloneToOwned + BencodeValueBufConstraint,
    <BufT as CloneToOwned>::Target: BencodeValueBufConstraint,
{
    type Target = BencodeValue<<BufT as CloneToOwned>::Target>;

    fn clone_to_owned(&self) -> Self::Target {
        match self {
            BencodeValue::Bytes(b) => BencodeValue::Bytes(b.clone_to_owned()),
            BencodeValue::Integer(i) => BencodeValue::Integer(*i),
            BencodeValue::List(l) => BencodeValue::List(l.clone_to_owned()),
            BencodeValue::Dict(d) => BencodeValue::Dict(d.clone_to_owned()),
        }
    }
}

pub type BencodeValueBorrowed<'a> = BencodeValue<ByteBuf<'a>>;
pub type BencodeValueOwned = BencodeValue<ByteString>;

#[cfg(test)]
mod tests {
    use crate::serde_bencode_ser::bencode_serialize_to_writer;

    use super::*;
    use serde::Serialize;
    use std::io::Read;

    #[test]
    fn test_deserialize_torrent_dyn() {
        let mut buf = Vec::new();
        let filename = "../librqbit/resources/ubuntu-21.04-desktop-amd64.iso.torrent";
        std::fs::File::open(filename)
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();

        let torrent_borrowed: BencodeValueBorrowed = from_bytes(&buf).unwrap();
        let torrent_owned: BencodeValueOwned = from_bytes(&buf).unwrap();
        dbg!(torrent_borrowed);
        dbg!(torrent_owned);
    }

    #[test]
    fn test_serialize_torrent_dyn() {
        let mut file_buf = Vec::new();
        let filename = "../librqbit/resources/ubuntu-21.04-desktop-amd64.iso.torrent";
        std::fs::File::open(filename)
            .unwrap()
            .read_to_end(&mut file_buf)
            .unwrap();

        let torrent: BencodeValueBorrowed = from_bytes(&file_buf).unwrap();

        let mut ser_buf = Vec::<u8>::new();
        bencode_serialize_to_writer(&torrent, &mut ser_buf).unwrap();

        let new_torrent = from_bytes(&ser_buf).unwrap();
        assert_eq!(torrent, new_torrent);
        assert_eq!(ser_buf, file_buf);
    }

    #[test]
    fn test_serialize_struct_with_option() {
        #[derive(Serialize)]
        struct Test {
            f1: i64,
            #[serde(skip_serializing_if = "Option::is_none")]
            missing: Option<i64>,
        }
        let test = Test {
            f1: 100,
            missing: None,
        };
        let mut buf = Vec::<u8>::new();
        bencode_serialize_to_writer(&test, &mut buf).unwrap();
        assert_eq!(&buf, b"d2:f1i100ee");
    }

    #[test]
    fn test_dict_ordering() -> anyhow::Result<()> {
        let hash_map = HashMap::from_iter((0..1000).map(|x| {
            (
                ByteString::from(x.to_string().into_bytes()),
                BencodeValue::Integer(x),
            )
        }));
        dbg!(&hash_map);
        let orig = BencodeValue::Dict(hash_map);
        dbg!(&orig);
        let first_buf: ByteString = orig.to_bytes()?.into();
        dbg!(&first_buf);
        let first_deser = from_bytes(&first_buf)?;
        assert_eq!(orig, first_deser);
        let second_buf = first_deser.to_bytes()?.into();
        assert_eq!(first_buf, second_buf);
        Ok(())
    }
}
