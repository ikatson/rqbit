// This crate used for making working with &[u8] or Vec<u8> generic in other parts of librqbit,
// for nicer display of binary data etc.
//
// Not useful outside of librqbit.

use std::borrow::Borrow;

use bytes::Bytes;
use serde::{Deserializer, Serialize};
use serde_derive::Deserialize;

use clone_to_owned::CloneToOwned;

#[derive(Default, PartialEq, Eq, Hash, Clone, PartialOrd, Ord)]
pub struct ByteBufOwned(pub bytes::Bytes);

#[derive(Default, Deserialize, PartialEq, Eq, Hash, Clone, Copy, PartialOrd, Ord)]
#[serde(transparent)]
pub struct ByteBuf<'a>(pub &'a [u8]);

pub trait ByteBufT:
    AsRef<[u8]>
    + Default
    + std::hash::Hash
    + Serialize
    + Eq
    + core::fmt::Debug
    + CloneToOwned
    + Borrow<[u8]>
{
}

impl ByteBufT for ByteBufOwned {}

impl ByteBufT for ByteBuf<'_> {}

struct HexBytes<'a>(&'a [u8]);
impl std::fmt::Display for HexBytes<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x?}")?;
        }
        Ok(())
    }
}

fn debug_bytes(b: &[u8], f: &mut std::fmt::Formatter<'_>, debug_strings: bool) -> std::fmt::Result {
    if b.is_empty() {
        return Ok(());
    }
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

impl std::fmt::Debug for ByteBuf<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        debug_bytes(self.0, f, true)
    }
}

impl std::fmt::Display for ByteBuf<'_> {
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

impl CloneToOwned for ByteBuf<'_> {
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
            } else {
                #[cfg(debug_assertions)]
                panic!("bug: broken buffers! not inside within_buffer");
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

impl std::convert::AsRef<[u8]> for ByteBuf<'_> {
    fn as_ref(&self) -> &[u8] {
        self.0
    }
}

impl std::convert::AsRef<[u8]> for ByteBufOwned {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl std::borrow::Borrow<[u8]> for ByteBufOwned {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}

impl std::borrow::Borrow<[u8]> for ByteBuf<'_> {
    fn borrow(&self) -> &[u8] {
        self.0
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

impl From<Bytes> for ByteBufOwned {
    fn from(b: Bytes) -> Self {
        Self(b)
    }
}

impl serde::ser::Serialize for ByteBuf<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(self.as_ref())
    }
}

impl serde::ser::Serialize for ByteBufOwned {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(self.as_ref())
    }
}

impl<'de> serde::de::Deserialize<'de> for ByteBufOwned {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
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
