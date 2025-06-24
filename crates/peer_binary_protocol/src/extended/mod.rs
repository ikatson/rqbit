use std::io::Cursor;

use bencode::BencodeValue;
use bencode::bencode_serialize_to_writer;
use buffers::ByteBuf;
use buffers::ByteBufT;
use byteorder::WriteBytesExt;
use serde::Deserialize;
use serde::Serialize;
use ut_pex::UtPex;

use crate::DoubleBufHelper;
use crate::MSGID_EXTENDED;
use crate::MY_EXTENDED_UT_PEX;

use self::{handshake::ExtendedHandshake, ut_metadata::UtMetadata};

use super::MessageDeserializeError;

pub mod handshake;
pub mod ut_metadata;
pub mod ut_pex;

use super::MY_EXTENDED_UT_METADATA;

#[derive(Debug, Default, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct PeerExtendedMessageIds {
    pub ut_metadata: Option<u8>,
    pub ut_pex: Option<u8>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ExtendedMessage<ByteBuf: ByteBufT> {
    Handshake(ExtendedHandshake<ByteBuf>),
    UtMetadata(UtMetadata<ByteBuf>),
    UtPex(UtPex<ByteBuf>),
    Dyn(u8, BencodeValue<ByteBuf>),
}

impl<'a> ExtendedMessage<ByteBuf<'a>> {
    pub fn serialize(
        &self,
        out: &mut [u8],
        peer_extended_msg_ids: &dyn Fn() -> PeerExtendedMessageIds,
    ) -> anyhow::Result<usize> {
        let mut out = Cursor::new(out);
        match self {
            ExtendedMessage::Dyn(msg_id, v) => {
                out.write_u8(*msg_id)?;
                bencode_serialize_to_writer(v, &mut out)?;
            }
            ExtendedMessage::Handshake(h) => {
                out.write_u8(0)?;
                bencode_serialize_to_writer(h, &mut out)?;
            }
            ExtendedMessage::UtMetadata(u) => {
                let emsg_id = peer_extended_msg_ids().ut_metadata.ok_or_else(|| {
                    anyhow::anyhow!("need peer's handshake to serialize ut_metadata")
                })?;
                out.write_u8(emsg_id)?;
                u.serialize(&mut out)?;
            }
            ExtendedMessage::UtPex(m) => {
                let emsg_id = peer_extended_msg_ids().ut_pex.ok_or_else(|| {
                    anyhow::anyhow!(
                        "need peer's handshake to serialize ut_pex, or peer does't support ut_pex"
                    )
                })?;
                out.write_u8(emsg_id)?;
                bencode_serialize_to_writer(m, &mut out)?;
            }
        }
        Ok(out.position() as usize)
    }

    pub fn deserialize(mut buf: DoubleBufHelper<'a>) -> Result<Self, MessageDeserializeError> {
        let msg_id = crate::MsgIdDebug(MSGID_EXTENDED);
        let emsg_id = buf
            .read_u8()
            .ok_or(MessageDeserializeError::NotEnoughData(1, Some(msg_id)))?;

        fn from_bytes_contig<'a, T>(buf: &DoubleBufHelper<'a>) -> Result<T, MessageDeserializeError>
        where
            T: serde::de::Deserialize<'a>,
        {
            let buf = buf
                .get_contiguous(buf.len())
                .ok_or(MessageDeserializeError::NeedContiguous)?;
            bencode::from_bytes(buf).map_err(|e| {
                tracing::trace!("error deserializing extended: {e:#}");
                MessageDeserializeError::Bencode(e.into_kind())
            })
        }

        match emsg_id {
            0 => Ok(ExtendedMessage::Handshake(from_bytes_contig(&buf)?)),
            MY_EXTENDED_UT_METADATA => {
                Ok(ExtendedMessage::UtMetadata(UtMetadata::deserialize(buf)?))
            }
            MY_EXTENDED_UT_PEX => Ok(ExtendedMessage::UtPex(from_bytes_contig(&buf)?)),
            _ => Ok(ExtendedMessage::Dyn(emsg_id, from_bytes_contig(&buf)?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use buffers::ByteBuf;

    use crate::{
        DoubleBufHelper, MY_EXTENDED_UT_METADATA, MY_EXTENDED_UT_PEX, MessageDeserializeError,
        extended::{
            ExtendedMessage, PeerExtendedMessageIds,
            ut_metadata::{UtMetadata, UtMetadataData},
        },
    };

    fn ut_metadata_trailing_bytes_is_error(msg: ExtendedMessage<ByteBuf>) {
        let mut buf = [0u8; 100];
        let sz = msg
            .serialize(&mut buf, &|| PeerExtendedMessageIds {
                ut_metadata: Some(MY_EXTENDED_UT_METADATA),
                ut_pex: Some(MY_EXTENDED_UT_PEX),
            })
            .unwrap();

        let deserialized =
            ExtendedMessage::deserialize(DoubleBufHelper::new(&buf[..sz], &[])).unwrap();
        assert_eq!(msg, deserialized);

        let res = ExtendedMessage::deserialize(DoubleBufHelper::new(&buf[..sz + 1], &[]));
        assert!(
            matches!(res, Err(MessageDeserializeError::UtMetadataTrailingBytes)),
            "expected trailing bytes error, got {res:?}"
        )
    }

    #[test]
    fn test_ut_metadata_trailing_bytes_is_error() {
        ut_metadata_trailing_bytes_is_error(ExtendedMessage::UtMetadata(UtMetadata::Request(42)));
        ut_metadata_trailing_bytes_is_error(ExtendedMessage::UtMetadata(UtMetadata::Reject(43)));
        ut_metadata_trailing_bytes_is_error(ExtendedMessage::UtMetadata(UtMetadata::Data(
            UtMetadataData::from_bytes(1, b"\x42\x42\x42\x42\x42"[..].into()),
        )));
    }

    #[test]
    fn test_ut_metadata_non_contiguous() {
        let mut buf = [0u8; 100];
        let msg = ExtendedMessage::UtMetadata(UtMetadata::Data(UtMetadataData::from_bytes(
            1,
            b"\x42\x42\x42\x42\x42"[..].into(),
        )));
        let sz = msg
            .serialize(&mut buf, &|| PeerExtendedMessageIds {
                ut_metadata: Some(MY_EXTENDED_UT_METADATA),
                ut_pex: Some(MY_EXTENDED_UT_PEX),
            })
            .unwrap();
        let bencode_sz = buf[..sz].iter().position(|byte| *byte == 0x42).unwrap();

        for split_point in 0..sz {
            let (d0, d1) = buf[..sz].split_at(split_point);
            let buf = DoubleBufHelper::new(d0, d1);
            let res = ExtendedMessage::deserialize(buf);
            if (2..bencode_sz).contains(&split_point) {
                assert!(
                    matches!(res, Err(MessageDeserializeError::NeedContiguous)),
                    "expected NeedContiguous, got {res:?}, split_point={split_point}, bencode_sz={bencode_sz}"
                );
                continue;
            }
            let de = res.unwrap();
            match de {
                ExtendedMessage::UtMetadata(UtMetadata::Data(d)) => {
                    assert_eq!(d.piece(), 1);
                    assert_eq!(d.len(), 5);
                    let mut debuf = [0u8; 5];
                    d.copy_to_slice(&mut debuf);
                    assert_eq!(debuf, b"\x42\x42\x42\x42\x42"[..]);
                }
                _ => panic!("bad msg"),
            }
        }
    }
}
