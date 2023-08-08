use std::io::Write;

use bencode::bencode_serialize_to_writer;
use bencode::BencodeDeserializer;
use clone_to_owned::CloneToOwned;
use serde::Deserialize;
use serde::Serialize;

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

impl<ByteBuf: CloneToOwned> CloneToOwned for UtMetadata<ByteBuf> {
    type Target = UtMetadata<<ByteBuf as CloneToOwned>::Target>;

    fn clone_to_owned(&self) -> Self::Target {
        match self {
            UtMetadata::Request(req) => UtMetadata::Request(*req),
            UtMetadata::Data {
                piece,
                total_size,
                data,
            } => UtMetadata::Data {
                piece: *piece,
                total_size: *total_size,
                data: data.clone_to_owned(),
            },
            UtMetadata::Reject(piece) => UtMetadata::Reject(*piece),
        }
    }
}

impl<'a, ByteBuf: 'a> UtMetadata<ByteBuf> {
    pub fn serialize(&self, buf: &mut Vec<u8>)
    where
        ByteBuf: AsRef<[u8]>,
    {
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
    pub fn deserialize(buf: &'a [u8]) -> Result<Self, MessageDeserializeError>
    where
        ByteBuf: From<&'a [u8]>,
    {
        let mut de = BencodeDeserializer::new_from_buf(buf);

        #[derive(Deserialize)]
        struct Message {
            msg_type: u32,
            piece: u32,
            total_size: Option<u32>,
        }

        let message =
            Message::deserialize(&mut de).map_err(|e| MessageDeserializeError::Other(e.into()))?;
        let remaining = de.into_remaining();

        match message.msg_type {
            // request
            0 => {
                if !remaining.is_empty() {
                    return Err(MessageDeserializeError::Other(anyhow::anyhow!(
                        "trailing bytes when decoding UtMetadata"
                    )));
                }
                Ok(UtMetadata::Request(message.piece))
            }
            // data
            1 => {
                let total_size = message.total_size.ok_or_else(|| {
                    MessageDeserializeError::Other(anyhow::anyhow!(
                        "expected key total_size to be present in UtMetadata \"data\" message"
                    ))
                })?;
                Ok(UtMetadata::Data {
                    piece: message.piece,
                    total_size,
                    data: ByteBuf::from(remaining),
                })
            }
            // reject
            2 => {
                if !remaining.is_empty() {
                    return Err(MessageDeserializeError::Other(anyhow::anyhow!(
                        "trailing bytes when decoding UtMetadata"
                    )));
                }
                Ok(UtMetadata::Reject(message.piece))
            }
            other => Err(MessageDeserializeError::Other(anyhow::anyhow!(
                "unrecognized ut_metadata message type {}",
                other
            ))),
        }
    }
}
