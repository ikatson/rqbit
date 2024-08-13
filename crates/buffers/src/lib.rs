// This crate used for making working with &[u8] or Vec<u8> generic in other parts of librqbit,
// for nicer display of binary data etc.
//
// Not useful outside of librqbit.

use bytes::Bytes;
use serde::{Deserialize, Deserializer};

use clone_to_owned::CloneToOwned;

#[derive(Default, PartialEq, Eq, Hash, Clone, PartialOrd, Ord)]
pub struct ByteBufOwned(pub bytes::Bytes);

#[derive(Default, Deserialize, PartialEq, Eq, Hash, Clone, PartialOrd, Ord)]
#[serde(transparent)]
pub struct ByteBuf<'a>(pub &'a [u8]);

pub trait ByteBufT {
    fn as_slice(&self) -> &[u8];
}

impl ByteBufT for ByteBufOwned {
    fn as_slice(&self) -> &[u8] {
        self.as_ref()
    }
}

impl<'a> ByteBufT for ByteBuf<'a> {
    fn as_slice(&self) -> &[u8] {
        self.as_ref()
    }
}

struct HexBytes<'a>(&'a [u8]);
impl<'a> std::fmt::Display for HexBytes<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x?}")?;
        }
        Ok(())
    }
}

fn debug_bytes(b: &[u8], f: &mut std::fmt::Formatter<'_>, debug_strings: bool) -> std::fmt::Result {
    if b.iter().all(|b| *b == 0) {
        return write!(f, "<{} bytes, all zeroes>", b.len());
    }
    match std::str::from_utf8(b) {
        Ok(s) => {
            if debug_strings {
                return write!(f, "{s:?}");
            } else {
                return write!(f, "{s}");
            }
        }
        Err(_e) => {}
    };

    // up to 20 bytes, display hex
    if b.len() <= 20 {
        return write!(f, "<{} bytes, 0x{}>", b.len(), HexBytes(b));
    }

    write!(f, "<{} bytes>", b.len())
}

impl<'a> std::fmt::Debug for ByteBuf<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_bytes(self.0, f, true)
    }
}

impl<'a> std::fmt::Display for ByteBuf<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_bytes(self.0, f, false)
    }
}

impl std::fmt::Debug for ByteBufOwned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_bytes(&self.0, f, true)
    }
}

impl std::fmt::Display for ByteBufOwned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_bytes(&self.0, f, false)
    }
}

impl<'a> CloneToOwned for ByteBuf<'a> {
    type Target = ByteBufOwned;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        // Try zero-copy from the provided buffer.
        if let Some(within_buffer) = within_buffer {
            let haystack = within_buffer.as_ptr() as usize;
            let haystack_end = haystack + within_buffer.len();
            let needle = self.0.as_ptr() as usize;
            let needle_end = needle + self.0.len();

            if needle >= haystack && needle_end <= haystack_end {
                return ByteBufOwned(within_buffer.slice_ref(self.0.as_ref()));
            }
        }

        ByteBufOwned(Bytes::copy_from_slice(self.0))
    }
}

impl CloneToOwned for ByteBufOwned {
    type Target = ByteBufOwned;

    fn clone_to_owned(&self, _within_buffer: Option<&Bytes>) -> Self::Target {
        ByteBufOwned(self.0.clone())
    }
}

impl<'a> std::convert::AsRef<[u8]> for ByteBuf<'a> {
    fn as_ref(&self) -> &[u8] {
        self.0
    }
}

impl std::convert::AsRef<[u8]> for ByteBufOwned {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl<'a> std::ops::Deref for ByteBuf<'a> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl std::ops::Deref for ByteBufOwned {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a> From<&'a [u8]> for ByteBuf<'a> {
    fn from(b: &'a [u8]) -> Self {
        Self(b)
    }
}

impl<'a> From<&'a [u8]> for ByteBufOwned {
    fn from(b: &'a [u8]) -> Self {
        Self(b.to_owned().into())
    }
}

impl From<Vec<u8>> for ByteBufOwned {
    fn from(b: Vec<u8>) -> Self {
        Self(b.into())
    }
}

impl<'a> serde::ser::Serialize for ByteBuf<'a> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(self.as_slice())
    }
}

impl serde::ser::Serialize for ByteBufOwned {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(self.as_slice())
    }
}

impl<'de> serde::de::Deserialize<'de> for ByteBufOwned {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = ByteBufOwned;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("byte string")
            }
            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(v.to_owned().into())
            }
        }
        deserializer.deserialize_byte_buf(Visitor {})
    }
}
