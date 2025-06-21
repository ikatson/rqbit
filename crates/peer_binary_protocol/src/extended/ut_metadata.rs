use bencode::BencodeDeserializer;
use bencode::bencode_serialize_to_writer;
use buffers::ByteBuf;
use buffers::ByteBufOwned;
use librqbit_core::constants::CHUNK_SIZE;
use serde::Deserialize;
use serde::Serialize;
use std::io::Write;

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
    pub fn serialize(&self, buf: &mut Vec<u8>) {
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
                bencode_serialize_to_writer(message, buf).unwrap()
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
                bencode_serialize_to_writer(message, buf).unwrap();
                buf.write_all(data.as_ref()).unwrap();
            }
            UtMetadata::Reject(piece) => {
                let message = Message {
                    msg_type: 2,
                    piece: *piece,
                    total_size: None,
                };
                bencode_serialize_to_writer(message, buf).unwrap();
            }
        }
    }

    pub fn deserialize(buf: &'a [u8]) -> Result<Self, MessageDeserializeError> {
        let mut de = BencodeDeserializer::new_from_buf(buf);

        #[derive(Deserialize)]
        struct Message {
            msg_type: u32,
            piece: u32,
            total_size: Option<u32>,
        }

        let message = Message::deserialize(&mut de)?;
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
