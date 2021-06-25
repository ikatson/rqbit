use bincode::Options;
use byteorder::ByteOrder;
use serde::{Deserialize, Serialize};

use crate::{
    buffers::{ByteBuf, ByteString},
    clone_to_owned::CloneToOwned,
};

const PREAMBLE_LEN: usize = 5;
const NO_PAYLOAD_MSG_LEN: usize = PREAMBLE_LEN;

const PSTR_BT1: &str = "BitTorrent protocol";

const LEN_PREFIX_KEEPALIVE: u32 = 0;
const LEN_PREFIX_CHOKE: u32 = 1;
const LEN_PREFIX_UNCHOKE: u32 = 1;
const LEN_PREFIX_INTERESTED: u32 = 1;
const LEN_PREFIX_NOT_INTERESTED: u32 = 1;
const LEN_PREFIX_HAVE: u32 = 5;
const LEN_PREFIX_REQUEST: u32 = 13;

const MSGID_CHOKE: u8 = 0;
const MSGID_UNCHOKE: u8 = 1;
const MSGID_INTERESTED: u8 = 2;
const MSGID_NOT_INTERESTED: u8 = 3;
const MSGID_HAVE: u8 = 4;
const MSGID_BITFIELD: u8 = 5;
const MSGID_REQUEST: u8 = 6;
const MSGID_PIECE: u8 = 7;

#[derive(Debug)]
pub enum MessageDeserializeError {
    NotEnoughData(usize, &'static str),
    UnsupportedMessageId(u8),
    IncorrectLenPrefix {
        received: u32,
        expected: u32,
        msg_id: u8,
    },
    OtherBincode {
        error: bincode::Error,
        msg_id: u8,
        len_prefix: u32,
        name: &'static str,
    },
}

#[derive(Debug)]
pub struct Piece<ByteBuf> {
    pub index: u32,
    pub begin: u32,
    pub block: ByteBuf,
}

impl<ByteBuf> Piece<ByteBuf>
where
    ByteBuf: AsRef<[u8]>,
{
    pub fn serialize(&self, buf: &mut [u8]) -> usize {
        byteorder::BigEndian::write_u32(&mut buf[0..4], self.index);
        byteorder::BigEndian::write_u32(&mut buf[4..8], self.begin);
        (&mut buf[8..8 + self.block.as_ref().len()]).copy_from_slice(self.block.as_ref());
        self.block.as_ref().len() + 8
    }
    pub fn deserialize<'a>(buf: &'a [u8]) -> Piece<ByteBuf>
    where
        ByteBuf: From<&'a [u8]> + 'a,
    {
        let index = byteorder::BigEndian::read_u32(&buf[0..4]);
        let begin = byteorder::BigEndian::read_u32(&buf[4..8]);
        let block = ByteBuf::from(&buf[8..]);
        Piece {
            index,
            begin,
            block,
        }
    }
}

impl std::fmt::Display for MessageDeserializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageDeserializeError::NotEnoughData(b, name) => {
                write!(
                    f,
                    "not enough data to deserialize {}: expected at least {} more bytes",
                    name, b
                )
            }
            MessageDeserializeError::UnsupportedMessageId(msg_id) => {
                write!(f, "unsupported message id {}", msg_id)
            }
            MessageDeserializeError::IncorrectLenPrefix {
                received,
                expected,
                msg_id,
            } => write!(
                f,
                "incorrect len prefix for message id {}, expected {}, received {}",
                msg_id, expected, received
            ),
            MessageDeserializeError::OtherBincode {
                error,
                msg_id,
                name,
                len_prefix,
            } => write!(
                f,
                "error deserializing {} (msg_id={}, len_prefix={}): {:?}",
                name, msg_id, len_prefix, error
            ),
        }
    }
}

impl std::error::Error for MessageDeserializeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MessageDeserializeError::OtherBincode { error, .. } => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum Message<ByteBuf> {
    Request(Request),
    Bitfield(ByteBuf),
    KeepAlive,
    Have(u32),
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Piece(Piece<ByteBuf>),
}

pub type MessageBorrowed<'a> = Message<ByteBuf<'a>>;
pub type MessageOwned = Message<ByteString>;

pub type BitfieldBorrowed<'a> = &'a bitvec::slice::BitSlice<bitvec::order::Lsb0, u8>;
pub type BitfieldOwned = bitvec::vec::BitVec<bitvec::order::Lsb0, u8>;

pub struct Bitfield<'a> {
    pub data: BitfieldBorrowed<'a>,
}

impl<ByteBuf: CloneToOwned> CloneToOwned for Message<ByteBuf> {
    type Target = Message<<ByteBuf as CloneToOwned>::Target>;

    fn clone_to_owned(&self) -> Self::Target {
        match self {
            Message::Request(req) => Message::Request(*req),
            Message::Bitfield(b) => Message::Bitfield(b.clone_to_owned()),
            Message::Choke => Message::Choke,
            Message::Unchoke => Message::Unchoke,
            Message::Interested => Message::Interested,
            Message::Piece(piece) => Message::Piece(Piece {
                index: piece.index,
                begin: piece.begin,
                block: piece.block.clone_to_owned(),
            }),
            Message::KeepAlive => Message::KeepAlive,
            Message::Have(v) => Message::Have(*v),
            Message::NotInterested => Message::NotInterested,
        }
    }
}

impl<'a> Bitfield<'a> {
    pub fn new_from_slice(buf: &'a [u8]) -> anyhow::Result<Self> {
        Ok(Self {
            data: bitvec::slice::BitSlice::from_slice(buf)?,
        })
    }
}

impl<'a> std::fmt::Debug for Bitfield<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bitfield")
            .field("_ones", &self.data.count_ones())
            .field("_len", &self.data.len())
            .finish()
    }
}

impl<ByteBuf> Message<ByteBuf>
where
    ByteBuf: AsRef<[u8]>,
{
    pub fn len_prefix_and_msg_id(&self) -> (u32, u8) {
        match self {
            Message::Request(_) => (LEN_PREFIX_REQUEST, MSGID_REQUEST),
            Message::Bitfield(b) => (1 + b.as_ref().len() as u32, MSGID_BITFIELD),
            Message::Choke => (LEN_PREFIX_CHOKE, MSGID_CHOKE),
            Message::Unchoke => (LEN_PREFIX_UNCHOKE, MSGID_UNCHOKE),
            Message::Interested => (LEN_PREFIX_INTERESTED, MSGID_INTERESTED),
            Message::NotInterested => (LEN_PREFIX_NOT_INTERESTED, MSGID_NOT_INTERESTED),
            Message::Piece(p) => (9 + p.block.as_ref().len() as u32, MSGID_PIECE),
            Message::KeepAlive => (LEN_PREFIX_KEEPALIVE, 0),
            Message::Have(_) => (LEN_PREFIX_HAVE, MSGID_HAVE),
        }
    }
    pub fn serialize(&self, out: &mut Vec<u8>) -> usize {
        let (lp, msg_id) = self.len_prefix_and_msg_id();

        out.resize(PREAMBLE_LEN, 0);

        byteorder::BigEndian::write_u32(&mut out[..4], lp);
        out[4] = msg_id;

        let ser = bopts();

        match self {
            Message::Request(request) => {
                const MSG_LEN: usize = PREAMBLE_LEN + 12;
                out.resize(MSG_LEN, 0);
                debug_assert_eq!((&out[PREAMBLE_LEN..]).len(), 12);
                ser.serialize_into(&mut out[PREAMBLE_LEN..], request)
                    .unwrap();
                MSG_LEN
            }
            Message::Bitfield(_) => todo!(),
            Message::Choke | Message::Unchoke | Message::Interested => PREAMBLE_LEN,
            Message::Piece(p) => {
                let msg_len = PREAMBLE_LEN + 8 + p.block.as_ref().len();
                out.resize(msg_len, 0);
                p.serialize(&mut out[PREAMBLE_LEN..(8 + p.block.as_ref().len())]);
                msg_len
            }
            Message::KeepAlive => 4,
            Message::Have(v) => {
                let msg_len = PREAMBLE_LEN + 4;
                out.resize(msg_len, 0);
                byteorder::BE::write_u32(&mut out[PREAMBLE_LEN..], *v);
                msg_len
            }
            Message::NotInterested => todo!(),
        }
    }
    pub fn deserialize<'a>(
        buf: &'a [u8],
    ) -> Result<(Message<ByteBuf>, usize), MessageDeserializeError>
    where
        ByteBuf: From<&'a [u8]> + 'a,
    {
        let len_prefix = match buf.get(0..4) {
            Some(bytes) => byteorder::BigEndian::read_u32(bytes),
            None => return Err(MessageDeserializeError::NotEnoughData(4, "message")),
        };
        if len_prefix == 0 {
            return Ok((Message::KeepAlive, 4));
        }

        let msg_id = match buf.get(4) {
            Some(msg_id) => *msg_id,
            None => return Err(MessageDeserializeError::NotEnoughData(1, "message")),
        };
        let rest = &buf[5..];
        let decoder_config = bincode::DefaultOptions::new()
            .with_fixint_encoding()
            .with_big_endian();

        match msg_id {
            MSGID_CHOKE => {
                if len_prefix != LEN_PREFIX_CHOKE {
                    return Err(MessageDeserializeError::IncorrectLenPrefix {
                        received: len_prefix,
                        expected: LEN_PREFIX_CHOKE,
                        msg_id,
                    });
                }
                Ok((Message::Choke, NO_PAYLOAD_MSG_LEN))
            }
            MSGID_UNCHOKE => {
                if len_prefix != LEN_PREFIX_UNCHOKE {
                    return Err(MessageDeserializeError::IncorrectLenPrefix {
                        received: len_prefix,
                        expected: LEN_PREFIX_UNCHOKE,
                        msg_id,
                    });
                }
                Ok((Message::Unchoke, NO_PAYLOAD_MSG_LEN))
            }
            MSGID_INTERESTED => {
                if len_prefix != LEN_PREFIX_INTERESTED {
                    return Err(MessageDeserializeError::IncorrectLenPrefix {
                        received: len_prefix,
                        expected: LEN_PREFIX_INTERESTED,
                        msg_id,
                    });
                }
                Ok((Message::Interested, NO_PAYLOAD_MSG_LEN))
            }
            MSGID_NOT_INTERESTED => {
                if len_prefix != LEN_PREFIX_NOT_INTERESTED {
                    return Err(MessageDeserializeError::IncorrectLenPrefix {
                        received: len_prefix,
                        expected: LEN_PREFIX_NOT_INTERESTED,
                        msg_id,
                    });
                }
                Ok((Message::NotInterested, NO_PAYLOAD_MSG_LEN))
            }
            MSGID_HAVE => {
                let expected_len = 4;
                match rest.get(..expected_len as usize) {
                    Some(h) => Ok((
                        Message::Have(byteorder::BE::read_u32(&h)),
                        PREAMBLE_LEN + expected_len,
                    )),
                    None => {
                        let missing = expected_len - rest.len();
                        Err(MessageDeserializeError::NotEnoughData(missing, "have"))
                    }
                }
            }
            MSGID_BITFIELD => {
                if len_prefix <= 1 {
                    return Err(MessageDeserializeError::IncorrectLenPrefix {
                        expected: 2,
                        received: len_prefix,
                        msg_id,
                    });
                }
                let expected_len = len_prefix as usize - 1;
                match rest.get(..expected_len as usize) {
                    Some(bitfield) => Ok((
                        Message::Bitfield(ByteBuf::from(bitfield)),
                        PREAMBLE_LEN + expected_len,
                    )),
                    None => {
                        let missing = expected_len - rest.len();
                        Err(MessageDeserializeError::NotEnoughData(missing, "bitfield"))
                    }
                }
            }
            MSGID_REQUEST => {
                let expected_len = 12;
                match rest.get(..expected_len as usize) {
                    Some(b) => {
                        let request = decoder_config.deserialize::<Request>(&b).unwrap();
                        Ok((Message::Request(request), PREAMBLE_LEN + expected_len))
                    }
                    None => {
                        let missing = expected_len - rest.len();
                        Err(MessageDeserializeError::NotEnoughData(missing, "request"))
                    }
                }
            }
            MSGID_PIECE => {
                if len_prefix <= 9 {
                    return Err(MessageDeserializeError::IncorrectLenPrefix {
                        expected: 10,
                        received: len_prefix,
                        msg_id,
                    });
                }
                // <len=0009+X> is for "9", "8" is for 2 integer fields in the piece.
                let expected_len = len_prefix as usize - 9 + 8;
                match rest.get(..expected_len) {
                    Some(b) => Ok((
                        Message::Piece(Piece::deserialize(&b)),
                        PREAMBLE_LEN + expected_len,
                    )),
                    None => Err(MessageDeserializeError::NotEnoughData(
                        expected_len - rest.len(),
                        "piece",
                    )),
                }
            }
            msg_id => Err(MessageDeserializeError::UnsupportedMessageId(msg_id)),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Handshake<'a> {
    pub pstr: &'a str,
    pub reserved: [u8; 8],
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
}

fn bopts() -> impl bincode::Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_big_endian()
}

impl<'a> Handshake<'a> {
    pub fn new(info_hash: [u8; 20], peer_id: [u8; 20]) -> Handshake<'static> {
        debug_assert_eq!(PSTR_BT1.len(), 19);
        Handshake {
            pstr: PSTR_BT1,
            reserved: [0; 8],
            info_hash,
            peer_id,
        }
    }
    fn bopts() -> impl bincode::Options {
        bincode::DefaultOptions::new()
    }
    pub fn deserialize(b: &[u8]) -> Result<(Handshake<'_>, usize), MessageDeserializeError> {
        let pstr_len = *b
            .get(0)
            .ok_or(MessageDeserializeError::NotEnoughData(1, "handshake"))?;
        let expected_len = 1usize + pstr_len as usize + 48;
        let hbuf = b
            .get(..expected_len)
            .ok_or(MessageDeserializeError::NotEnoughData(
                expected_len,
                "handshake",
            ))?;
        Ok((Self::bopts().deserialize(&hbuf).unwrap(), expected_len))
    }
    pub fn serialize(&self) -> Vec<u8> {
        Self::bopts().serialize(&self).unwrap()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct Request {
    pub index: u32,
    pub begin: u32,
    pub length: u32,
}

impl Request {
    pub fn new(index: u32, begin: u32, length: u32) -> Self {
        Self {
            index,
            begin,
            length,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_handshake_serialize() {
        let info_hash = [
            1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ];
        let peer_id = [
            1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ];
        let b = dbg!(Handshake::new(info_hash, peer_id).serialize());
        assert_eq!(b.len(), 20 + 20 + 8 + 19 + 1);
    }
}
