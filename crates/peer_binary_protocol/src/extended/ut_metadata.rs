use bencode::BencodeDeserializer;
use bencode::bencode_serialize_to_writer;
use buffers::ByteBuf;
use buffers::ByteBufOwned;
use buffers::ByteBufT;
use librqbit_core::constants::CHUNK_SIZE;
use serde::Deserialize;
use serde_derive::Serialize;
use std::io::Cursor;
use std::io::Write;

use crate::DoubleBufHelper;
use crate::MessageDeserializeError;
use crate::SerializeError;

#[derive(Debug, Eq, PartialEq)]
pub struct UtMetadataData<ByteBuf> {
    piece: u32,
    total_size: u32,
    data_0: ByteBuf,
    data_1: ByteBuf,
}

impl<ByteBuf: ByteBufT> UtMetadataData<ByteBuf> {
    pub fn from_bytes(piece: u32, total_size: u32, data: ByteBuf) -> Self {
        Self {
            piece,
            total_size,
            data_0: data,
            data_1: Default::default(),
        }
    }
}

impl<'a> UtMetadataData<ByteBuf<'a>> {
    pub fn piece(&self) -> u32 {
        self.piece
    }

    pub fn len(&self) -> usize {
        self.data_0.as_ref().len() + self.data_1.as_ref().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data_0.as_ref().is_empty() && self.data_1.as_ref().is_empty()
    }

    pub fn as_double_buf(&self) -> DoubleBufHelper<'a> {
        DoubleBufHelper::new(self.data_0.0, self.data_1.0)
    }

    pub fn copy_to_slice(&self, mut out: &mut [u8]) {
        let d0 = self.data_0.as_ref();
        let d1 = self.data_1.as_ref();
        out[..d0.len()].copy_from_slice(d0);
        out = &mut out[d0.len()..];
        out[..d1.len()].copy_from_slice(d1);
    }

    fn validate(&self) -> Result<(), MessageDeserializeError> {
        if self.total_size == 0 {
            return Err(MessageDeserializeError::UtMetadataMissingTotalSize);
        }
        if self.len() > CHUNK_SIZE as usize {
            return Err(MessageDeserializeError::UtMetadataTooLarge(
                self.len() as u32
            ));
        }
        let total_pieces = self.total_size.div_ceil(CHUNK_SIZE);
        if self.piece >= total_pieces {
            return Err(MessageDeserializeError::UtMetadataPieceOutOfBounds {
                total_pieces,
                received_piece: self.piece,
            });
        }
        let expected_size = self
            .total_size
            .saturating_sub(self.piece * CHUNK_SIZE)
            .min(CHUNK_SIZE);
        if self.len() as u32 != expected_size {
            return Err(MessageDeserializeError::UtMetadataSizeMismatch {
                expected_size,
                received_size: self.len() as u32,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum UtMetadata<ByteBuf> {
    Request(u32),
    Data(UtMetadataData<ByteBuf>),
    Reject(u32),
}

impl UtMetadata<ByteBufOwned> {
    pub fn as_borrowed(&self) -> UtMetadata<ByteBuf<'_>> {
        match self {
            UtMetadata::Request(req) => UtMetadata::Request(*req),
            UtMetadata::Data(d) => UtMetadata::Data(UtMetadataData {
                piece: d.piece,
                data_0: ByteBuf::from(d.data_0.as_ref()),
                data_1: ByteBuf::from(d.data_1.as_ref()),
                total_size: d.total_size,
            }),
            UtMetadata::Reject(r) => UtMetadata::Reject(*r),
        }
    }
}

impl<'a> UtMetadata<ByteBuf<'a>> {
    pub fn serialize(&self, writer: &mut Cursor<&mut [u8]>) -> Result<(), SerializeError> {
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
            UtMetadata::Data(d) => {
                let message = Message {
                    msg_type: 1,
                    piece: d.piece,
                    total_size: Some(d.total_size),
                };
                bencode_serialize_to_writer(message, writer)?;
                writer.write_all(d.data_0.as_ref())?;
                writer.write_all(d.data_1.as_ref())?;
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

    pub fn deserialize(mut buf: DoubleBufHelper<'a>) -> Result<Self, MessageDeserializeError> {
        #[derive(serde_derive::Deserialize)]
        struct UtMetadataMsg {
            msg_type: u32,
            piece: u32,
            total_size: Option<u32>,
        }

        const MAX_BMSG_SIZE: usize =
            b"d8:msg_typei10e5:piecei4294967296e10:total_sizei16384ee".len();
        let (contig, is_contig) = match buf.get_contiguous(MAX_BMSG_SIZE.min(buf.len())) {
            Some(c) => (c, true),
            None => (buf.get()[0], false),
        };

        let mut de = BencodeDeserializer::new_from_buf(contig);
        let message = match UtMetadataMsg::deserialize(&mut de) {
            Ok(message) => {
                let consumed = contig.len() - de.into_remaining().len();
                buf.advance(consumed);
                message
            }
            Err(e) => {
                if is_contig {
                    return Err(MessageDeserializeError::Bencode(e));
                }
                return Err(MessageDeserializeError::NeedContiguous);
            }
        };

        match message.msg_type {
            // request
            0 => {
                if !buf.is_empty() {
                    return Err(MessageDeserializeError::UtMetadataTrailingBytes);
                }
                Ok(UtMetadata::Request(message.piece))
            }
            // data
            1 => {
                let total_size = message
                    .total_size
                    .ok_or(MessageDeserializeError::UtMetadataMissingTotalSize)?;

                let [data_0, data_1] = buf.get();
                let d = UtMetadataData {
                    piece: message.piece,
                    data_0: data_0.into(),
                    data_1: data_1.into(),
                    total_size,
                };
                d.validate()?;
                Ok(UtMetadata::Data(d))
            }
            // reject
            2 => {
                if !buf.is_empty() {
                    return Err(MessageDeserializeError::UtMetadataTrailingBytes);
                }
                Ok(UtMetadata::Reject(message.piece))
            }
            other => Err(MessageDeserializeError::UtMetadataTypeUnknown(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use buffers::ByteBuf;
    use librqbit_core::constants::CHUNK_SIZE;

    use crate::{MessageDeserializeError, extended::ut_metadata::UtMetadataData};

    #[test]
    fn test_ut_metadata_validate() {
        // success
        UtMetadataData::from_bytes(0, 3, ByteBuf(&b"foo"[..]))
            .validate()
            .unwrap();

        // 0 size
        let err = UtMetadataData::from_bytes(0, 0, ByteBuf(&[]))
            .validate()
            .unwrap_err();
        assert!(
            matches!(err, MessageDeserializeError::UtMetadataMissingTotalSize),
            "{:?}",
            err
        );

        // 0 provided size
        let err = UtMetadataData::from_bytes(0, 0, ByteBuf(&b"foo"[..]))
            .validate()
            .unwrap_err();
        assert!(
            matches!(err, MessageDeserializeError::UtMetadataMissingTotalSize),
            "{:?}",
            err
        );

        // piece out of bounds
        let err = UtMetadataData::from_bytes(1, 3, ByteBuf(&b"foo"[..]))
            .validate()
            .unwrap_err();
        assert!(
            matches!(
                err,
                MessageDeserializeError::UtMetadataPieceOutOfBounds {
                    total_pieces: 1,
                    received_piece: 1
                }
            ),
            "{:?}",
            err
        );

        // piece out of bounds
        let err = UtMetadataData::from_bytes(0, 3, ByteBuf(&b"foobar"[..]))
            .validate()
            .unwrap_err();
        assert!(
            matches!(
                err,
                MessageDeserializeError::UtMetadataSizeMismatch {
                    expected_size: 3,
                    received_size: 6
                }
            ),
            "{:?}",
            err
        );

        // success large piece
        UtMetadataData::from_bytes(0, CHUNK_SIZE + 1, ByteBuf(&[0u8; CHUNK_SIZE as usize][..]))
            .validate()
            .unwrap();
        UtMetadataData::from_bytes(1, CHUNK_SIZE + 1, ByteBuf(&[0u8; 1][..]))
            .validate()
            .unwrap();

        // piece out of bounds
        let err =
            UtMetadataData::from_bytes(2, CHUNK_SIZE + 1, ByteBuf(&[0u8; CHUNK_SIZE as usize][..]))
                .validate()
                .unwrap_err();
        assert!(
            matches!(
                err,
                MessageDeserializeError::UtMetadataPieceOutOfBounds {
                    total_pieces: 2,
                    received_piece: 2
                }
            ),
            "{:?}",
            err
        );

        // wrong size
        let err = UtMetadataData::from_bytes(
            0,
            CHUNK_SIZE + 1,
            ByteBuf(&[0u8; CHUNK_SIZE as usize - 1][..]),
        )
        .validate()
        .unwrap_err();

        assert!(
            matches!(
                err,
                MessageDeserializeError::UtMetadataSizeMismatch {
                    expected_size,
                    received_size,
                } if expected_size == CHUNK_SIZE && received_size == CHUNK_SIZE - 1
            ),
            "{:?}",
            err
        );
    }
}
