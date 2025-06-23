use bencode::BencodeDeserializer;
use bencode::bencode_serialize_to_writer;
use buffers::ByteBuf;
use buffers::ByteBufOwned;
use librqbit_core::constants::CHUNK_SIZE;
use serde::Deserialize;
use serde::Serialize;
use std::io::Cursor;
use std::io::Write;

use crate::DoubleBufHelper;
use crate::MAX_MSG_LEN;
use crate::MessageDeserializeError;

#[derive(Debug)]
pub enum UtMetadata<ByteBuf> {
    Request(u32),
    Data {
        piece: u32,
        total_size: u32,
        data: ByteBuf,
    },
    Reject(u32),
}

impl UtMetadata<ByteBufOwned> {
    pub fn as_borrowed(&self) -> UtMetadata<ByteBuf> {
        match self {
            UtMetadata::Request(req) => UtMetadata::Request(*req),
            UtMetadata::Data {
                piece,
                total_size,
                data,
            } => UtMetadata::Data {
                piece: *piece,
                total_size: *total_size,
                data: ByteBuf::from(data.as_ref()),
            },
            UtMetadata::Reject(r) => UtMetadata::Reject(*r),
        }
    }
}

impl<'a> UtMetadata<ByteBuf<'a>> {
    pub fn serialize(&self, writer: &mut Cursor<&mut [u8]>) -> anyhow::Result<()> {
        #[derive(Serialize)]
        struct Message {
            msg_type: u32,
            piece: u32,
            #[serde(skip_serializing_if = "Option::is_none")]
            total_size: Option<u32>,
        }
        match self {
            UtMetadata::Request(piece) => {
                let message = Message {
                    msg_type: 0,
                    piece: *piece,
                    total_size: None,
                };
                bencode_serialize_to_writer(message, writer)?
            }
            UtMetadata::Data {
                piece,
                total_size,
                data,
            } => {
                let message = Message {
                    msg_type: 1,
                    piece: *piece,
                    total_size: Some(*total_size),
                };
                bencode_serialize_to_writer(message, writer)?;
                writer.write_all(data.as_ref())?;
            }
            UtMetadata::Reject(piece) => {
                let message = Message {
                    msg_type: 2,
                    piece: *piece,
                    total_size: None,
                };
                bencode_serialize_to_writer(message, writer)?;
            }
        }
        Ok(())
    }

    pub fn deserialize(
        buf: &mut DoubleBufHelper<'a>,
        msg_len: usize,
    ) -> Result<Self, MessageDeserializeError> {
        //

        #[derive(Deserialize)]
        struct UtMetadataMsg {
            msg_type: u32,
            piece: u32,
            total_size: Option<u32>,
        }

        const MAX_BMSG_SIZE: usize =
            b"d8:msg_typei10e5:piecei4294967296e10:total_sizei16384ee".len();
        let (contig, is_contig) = match buf.get_contiguous(MAX_BMSG_SIZE) {
            Some(c) => (c, true),
            None => (buf.get().0, false),
        };

        let mut de = BencodeDeserializer::new_from_buf(contig);
        let message = match UtMetadataMsg::deserialize(&mut de) {
            Ok(message) => message,
            Err(e) => {
                if is_contig {
                    return Err(MessageDeserializeError::Bencode(e));
                }
                return Err(MessageDeserializeError::NeedContiguous);
            }
        };
        let remaining = de.into_remaining().len();
        let remaining = de.into_remaining();

        match message.msg_type {
            // request
            0 => {
                if !remaining.is_empty() {
                    return Err(MessageDeserializeError::UtMetadataTrailingBytes);
                }
                Ok(UtMetadata::Request(message.piece))
            }
            // data
            1 => {
                let total_size = message
                    .total_size
                    .ok_or(MessageDeserializeError::UtMetadataMissingTotalSize)?;
                if remaining.len() != total_size as usize {
                    return Err(MessageDeserializeError::UtMetadataSizeMismatch {
                        total_size,
                        received_len: buf.len() as u32,
                    });
                }
                if total_size > CHUNK_SIZE {
                    return Err(MessageDeserializeError::UtMetadataTooLarge(total_size));
                }
                Ok(UtMetadata::Data {
                    piece: message.piece,
                    total_size,
                    data: ByteBuf::from(remaining),
                })
            }
            // reject
            2 => {
                if !remaining.is_empty() {
                    return Err(MessageDeserializeError::UtMetadataTrailingBytes);
                }
                Ok(UtMetadata::Reject(message.piece))
            }
            other => Err(MessageDeserializeError::UtMetadataTypeUnknown(other)),
        }
    }
}
