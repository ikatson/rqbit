use bencode::BencodeValue;
use bencode::bencode_serialize_to_writer;
use bencode::from_bytes;
use buffers::ByteBuf;
use buffers::ByteBufT;
use serde::Deserialize;
use serde::Serialize;
use ut_pex::UtPex;

use crate::MY_EXTENDED_UT_PEX;

use self::{handshake::ExtendedHandshake, ut_metadata::UtMetadata};

use super::MessageDeserializeError;

pub mod handshake;
pub mod ut_metadata;
pub mod ut_pex;

use super::MY_EXTENDED_UT_METADATA;

#[derive(Debug, Default, Serialize, Deserialize, Clone, Copy)]
pub struct PeerExtendedMessageIds {
    pub ut_metadata: Option<u8>,
    pub ut_pex: Option<u8>,
}

#[derive(Debug)]
pub enum ExtendedMessage<ByteBuf: ByteBufT> {
    Handshake(ExtendedHandshake<ByteBuf>),
    UtMetadata(UtMetadata<ByteBuf>),
    UtPex(UtPex<ByteBuf>),
    Dyn(u8, BencodeValue<ByteBuf>),
}

impl<'a> ExtendedMessage<ByteBuf<'a>> {
    pub fn serialize(
        &self,
        out: &mut Vec<u8>,
        extended_handshake_ut_metadata: &dyn Fn() -> PeerExtendedMessageIds,
    ) -> anyhow::Result<()> {
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
                let emsg_id = extended_handshake_ut_metadata()
                    .ut_metadata
                    .ok_or_else(|| {
                        anyhow::anyhow!("need peer's handshake to serialize ut_metadata")
                    })?;
                out.push(emsg_id);
                u.serialize(out);
            }
            ExtendedMessage::UtPex(m) => {
                let emsg_id = extended_handshake_ut_metadata().ut_pex.ok_or_else(|| {
                    anyhow::anyhow!(
                        "need peer's handshake to serialize ut_pex, or peer does't support ut_pex"
                    )
                })?;
                out.push(emsg_id);
                bencode_serialize_to_writer(m, out)?;
            }
        }
        Ok(())
    }

    pub fn deserialize_unchecked_len(mut buf: &'a [u8]) -> Result<Self, MessageDeserializeError> {
        let emsg_id = buf[0];
        buf = &buf[1..];

        match emsg_id {
            0 => Ok(ExtendedMessage::Handshake(from_bytes(buf)?)),
            MY_EXTENDED_UT_METADATA => {
                Ok(ExtendedMessage::UtMetadata(UtMetadata::deserialize(buf)?))
            }
            MY_EXTENDED_UT_PEX => Ok(ExtendedMessage::UtPex(from_bytes(buf)?)),
            _ => Ok(ExtendedMessage::Dyn(emsg_id, from_bytes(buf)?)),
        }
    }
}
