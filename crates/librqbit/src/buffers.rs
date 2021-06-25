use serde::Deserialize;

use crate::clone_to_owned::CloneToOwned;

#[derive(PartialEq, Eq, Hash, Clone)]
pub struct ByteString(pub Vec<u8>);

impl std::fmt::Debug for ByteString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.iter().all(|b| *b == 0) {
            return write!(f, "<{} bytes, all zeroes>", self.0.len());
        }
        match std::str::from_utf8(self.0.as_slice()) {
            Ok(bytes) => bytes.fmt(f),
            Err(_e) => write!(f, "<{} bytes>", self.0.len()),
        }
    }
}

#[derive(Deserialize, PartialEq, Eq, Hash, Clone)]
#[serde(transparent)]
pub struct ByteBuf<'a>(pub &'a [u8]);

impl<'a> ByteBuf<'a> {
    pub fn as_bytes(&'a self) -> &'a [u8] {
        self.0
    }
}

fn debug_raw_bytes(b: &[u8], f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "<{} bytes>", b.len())
}

impl<'a> std::fmt::Debug for ByteBuf<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.iter().all(|b| *b == 0) {
            return write!(f, "<{} bytes, all zeroes>", self.0.len());
        }
        match std::str::from_utf8(self.0) {
            Ok(bytes) => bytes.fmt(f),
            Err(_e) => debug_raw_bytes(&self.0, f),
        }
    }
}

impl<'a> std::fmt::Display for ByteBuf<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.iter().all(|b| *b == 0) {
            return write!(f, "<{} bytes, all zeroes>", self.0.len());
        }
        match std::str::from_utf8(self.0) {
            Ok(bytes) => f.write_str(bytes),
            Err(_e) => debug_raw_bytes(&self.0, f),
        }
    }
}

impl<'a> CloneToOwned for ByteBuf<'a> {
    type Target = ByteString;

    fn clone_to_owned(&self) -> Self::Target {
        ByteString(self.0.into())
    }
}

impl CloneToOwned for ByteString {
    type Target = ByteString;

    fn clone_to_owned(&self) -> Self::Target {
        self.clone()
    }
}

impl<'a> std::convert::AsRef<[u8]> for ByteBuf<'a> {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl std::convert::AsRef<[u8]> for ByteString {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl<'a> std::ops::Deref for ByteBuf<'a> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::Deref for ByteString {
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

impl<'a> From<&'a [u8]> for ByteString {
    fn from(b: &'a [u8]) -> Self {
        Self(b.into())
    }
}

impl From<Vec<u8>> for ByteString {
    fn from(b: Vec<u8>) -> Self {
        Self(b)
    }
}
