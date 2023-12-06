use bencode::bencode_serialize_to_writer;
use bencode::from_bytes;
use bencode::BencodeValue;
use clone_to_owned::CloneToOwned;
use serde::{Deserialize, Serialize};

use self::{handshake::ExtendedHandshake, ut_metadata::UtMetadata};

use super::MessageDeserializeError;

pub mod handshake;
pub mod ut_metadata;

use super::MY_EXTENDED_UT_METADATA;

#[derive(Debug)]
pub enum ExtendedMessage<ByteBuf: std::hash::Hash + Eq> {
    Handshake(ExtendedHandshake<ByteBuf>),
    UtMetadata(UtMetadata<ByteBuf>),
    Dyn(u8, BencodeValue<ByteBuf>),
}

impl<ByteBuf> CloneToOwned for ExtendedMessage<ByteBuf>
where
    ByteBuf: CloneToOwned + std::hash::Hash + Eq,
    <ByteBuf as CloneToOwned>::Target: std::hash::Hash + Eq,
{
    type Target = ExtendedMessage<<ByteBuf as CloneToOwned>::Target>;

    fn clone_to_owned(&self) -> Self::Target {
        match self {
            ExtendedMessage::Handshake(h) => ExtendedMessage::Handshake(h.clone_to_owned()),
            ExtendedMessage::Dyn(u, d) => ExtendedMessage::Dyn(*u, d.clone_to_owned()),
            ExtendedMessage::UtMetadata(m) => ExtendedMessage::UtMetadata(m.clone_to_owned()),
        }
    }
}

impl<'a, ByteBuf: 'a + std::hash::Hash + Eq + Serialize> ExtendedMessage<ByteBuf> {
    pub fn serialize(
        &self,
        out: &mut Vec<u8>,
        extended_handshake_ut_metadata: &dyn Fn() -> Option<u8>,
    ) -> anyhow::Result<()>
    where
        ByteBuf: AsRef<[u8]>,
    {
        match self {
            ExtendedMessage::Dyn(msg_id, v) => {
                out.push(*msg_id);
                bencode_serialize_to_writer(v, out)?;
            }
            ExtendedMessage::Handshake(h) => {
                out.push(0);
                bencode_serialize_to_writer(h, out)?;
            }
            ExtendedMessage::UtMetadata(u) => {
                let emsg_id = extended_handshake_ut_metadata().ok_or_else(|| {
                    anyhow::anyhow!("need peer's handshake to serialize ut_metadata")
                })?;
                out.push(emsg_id);
                u.serialize(out);
            }
        }
        Ok(())
    }

    pub fn deserialize(mut buf: &'a [u8]) -> Result<Self, MessageDeserializeError>
    where
        ByteBuf: Deserialize<'a> + From<&'a [u8]>,
    {
        let emsg_id = buf.first().copied().ok_or_else(|| {
            MessageDeserializeError::Other(anyhow::anyhow!(
                "cannot deserialize extended message: can't read first byte"
            ))
        })?;

        buf = buf.get(1..).ok_or_else(|| {
            MessageDeserializeError::Other(anyhow::anyhow!(
                "cannot deserialize extended message: buffer empty"
            ))
        })?;

        match emsg_id {
            0 => Ok(ExtendedMessage::Handshake(from_bytes(buf)?)),
            MY_EXTENDED_UT_METADATA => {
                Ok(ExtendedMessage::UtMetadata(UtMetadata::deserialize(buf)?))
            }
            _ => Ok(ExtendedMessage::Dyn(emsg_id, from_bytes(buf)?)),
        }
    }
}
