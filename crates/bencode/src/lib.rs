#![warn(clippy::used_underscore_binding)]

mod bencode_value;
mod raw_value;
mod serde_bencode_de;
mod serde_bencode_ser;

pub use buffers::{ByteBuf, ByteString};

pub use bencode_value::*;
pub use raw_value::*;
pub use serde_bencode_de::*;
pub use serde_bencode_ser::*;

use std::collections::BTreeMap;
use std::fmt::Formatter;
use std::marker::PhantomData;

use serde::{
    de::{value::BorrowedBytesDeserializer, DeserializeOwned, Error as _, MapAccess, Visitor},
    ser::{Impossible},
    Deserialize, Deserializer, Serialize, Serializer,
};

use clone_to_owned::CloneToOwned;

fn escape_bytes(bytes: &[u8]) -> String {
    String::from_utf8(
        bytes
            .iter()
            .copied()
            .flat_map(std::ascii::escape_default)
            .collect(),
    )
    .unwrap()
}
