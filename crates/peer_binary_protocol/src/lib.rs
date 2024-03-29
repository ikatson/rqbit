// BitTorrent peer protocol implementation: parsing, serialization etc.
//
// Can be used outside of librqbit.

pub mod extended;

use bincode::Options;
use buffers::{ByteBuf, ByteString};
use byteorder::{ByteOrder, BE};
use clone_to_owned::CloneToOwned;
use librqbit_core::{constants::CHUNK_SIZE, hash_id::Id20, lengths::ChunkInfo};
use serde::{Deserialize, Serialize};

use self::extended::ExtendedMessage;

const INTEGER_LEN: usize = 4;
const MSGID_LEN: usize = 1;
const PREAMBLE_LEN: usize = INTEGER_LEN + MSGID_LEN;
const PIECE_MESSAGE_PREAMBLE_LEN: usize = PREAMBLE_LEN + INTEGER_LEN * 2;
pub const PIECE_MESSAGE_DEFAULT_LEN: usize = PIECE_MESSAGE_PREAMBLE_LEN + CHUNK_SIZE as usize;

const NO_PAYLOAD_MSG_LEN: usize = PREAMBLE_LEN;

const PSTR_BT1: &str = "BitTorrent protocol";

const LEN_PREFIX_KEEPALIVE: u32 = 0;
const LEN_PREFIX_CHOKE: u32 = 1;
const LEN_PREFIX_UNCHOKE: u32 = 1;
const LEN_PREFIX_INTERESTED: u32 = 1;
const LEN_PREFIX_NOT_INTERESTED: u32 = 1;
const LEN_PREFIX_HAVE: u32 = 5;
const LEN_PREFIX_PIECE: u32 = 9;
const LEN_PREFIX_REQUEST: u32 = 13;

const MSGID_CHOKE: u8 = 0;
const MSGID_UNCHOKE: u8 = 1;
const MSGID_INTERESTED: u8 = 2;
const MSGID_NOT_INTERESTED: u8 = 3;
const MSGID_HAVE: u8 = 4;
const MSGID_BITFIELD: u8 = 5;
const MSGID_REQUEST: u8 = 6;
const MSGID_PIECE: u8 = 7;
const MSGID_CANCEL: u8 = 8;
const MSGID_EXTENDED: u8 = 20;

pub const MY_EXTENDED_UT_METADATA: u8 = 3;

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
    Other(anyhow::Error),
}

pub fn serialize_piece_preamble(chunk: &ChunkInfo, mut buf: &mut [u8]) -> usize {
    BE::write_u32(&mut buf[0..4], LEN_PREFIX_PIECE + chunk.size);
    buf[4] = MSGID_PIECE;

    buf = &mut buf[PREAMBLE_LEN..];
    BE::write_u32(&mut buf[0..4], chunk.piece_index.get());
    BE::write_u32(&mut buf[4..8], chunk.offset);

    PIECE_MESSAGE_PREAMBLE_LEN
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
    pub fn from_data<T>(index: u32, begin: u32, block: T) -> Piece<ByteBuf>
    where
        ByteBuf: From<T>,
    {
        Piece {
            index,
            begin,
            block: ByteBuf::from(block),
        }
    }

    pub fn serialize(&self, mut buf: &mut [u8]) -> usize {
        byteorder::BigEndian::write_u32(&mut buf[0..4], self.index);
        byteorder::BigEndian::write_u32(&mut buf[4..8], self.begin);
        buf = &mut buf[8..];
        buf.copy_from_slice(self.block.as_ref());
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
                    "not enough data to deserialize {name}: expected at least {b} more bytes"
                )
            }
            MessageDeserializeError::UnsupportedMessageId(msg_id) => {
                write!(f, "unsupported message id {msg_id}")
            }
            MessageDeserializeError::IncorrectLenPrefix {
                received,
                expected,
                msg_id,
            } => write!(
                f,
                "incorrect len prefix for message id {msg_id}, expected {expected}, received {received}"
            ),
            MessageDeserializeError::OtherBincode {
                error,
                msg_id,
                name,
                len_prefix,
            } => write!(
                f,
                "error deserializing {name} (msg_id={msg_id}, len_prefix={len_prefix}): {error:?}"
            ),
            MessageDeserializeError::Other(e) => write!(f, "{e}"),
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

impl From<anyhow::Error> for MessageDeserializeError {
    fn from(e: anyhow::Error) -> Self {
        MessageDeserializeError::Other(e)
    }
}

#[derive(Debug)]
pub enum Message<ByteBuf: std::hash::Hash + Eq> {
    Request(Request),
    Cancel(Request),
    Bitfield(ByteBuf),
    KeepAlive,
    Have(u32),
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Piece(Piece<ByteBuf>),
    Extended(ExtendedMessage<ByteBuf>),
}

pub type MessageBorrowed<'a> = Message<ByteBuf<'a>>;
pub type MessageOwned = Message<ByteString>;

pub type BitfieldBorrowed<'a> = &'a bitvec::slice::BitSlice<u8, bitvec::order::Lsb0>;
pub type BitfieldOwned = bitvec::vec::BitVec<u8, bitvec::order::Lsb0>;

pub struct Bitfield<'a> {
    pub data: BitfieldBorrowed<'a>,
}

impl<ByteBuf> CloneToOwned for Message<ByteBuf>
where
    ByteBuf: CloneToOwned + std::hash::Hash + Eq,
    <ByteBuf as CloneToOwned>::Target: std::hash::Hash + Eq,
{
    type Target = Message<<ByteBuf as CloneToOwned>::Target>;

    fn clone_to_owned(&self) -> Self::Target {
        match self {
            Message::Request(req) => Message::Request(*req),
            Message::Cancel(req) => Message::Cancel(*req),
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
            Message::Extended(e) => Message::Extended(e.clone_to_owned()),
        }
    }
}

impl<'a> Bitfield<'a> {
    pub fn new_from_slice(buf: &'a [u8]) -> anyhow::Result<Self> {
        Ok(Self {
            data: bitvec::slice::BitSlice::from_slice(buf),
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
    ByteBuf: AsRef<[u8]> + std::hash::Hash + Eq + Serialize,
{
    pub fn len_prefix_and_msg_id(&self) -> (u32, u8) {
        match self {
            Message::Request(_) | Message::Cancel(_) => (LEN_PREFIX_REQUEST, MSGID_REQUEST),
            Message::Bitfield(b) => (1 + b.as_ref().len() as u32, MSGID_BITFIELD),
            Message::Choke => (LEN_PREFIX_CHOKE, MSGID_CHOKE),
            Message::Unchoke => (LEN_PREFIX_UNCHOKE, MSGID_UNCHOKE),
            Message::Interested => (LEN_PREFIX_INTERESTED, MSGID_INTERESTED),
            Message::NotInterested => (LEN_PREFIX_NOT_INTERESTED, MSGID_NOT_INTERESTED),
            Message::Piece(p) => (
                LEN_PREFIX_PIECE + p.block.as_ref().len() as u32,
                MSGID_PIECE,
            ),
            Message::KeepAlive => (LEN_PREFIX_KEEPALIVE, 0),
            Message::Have(_) => (LEN_PREFIX_HAVE, MSGID_HAVE),
            Message::Extended(_) => (0, MSGID_EXTENDED),
        }
    }
    pub fn serialize(
        &self,
        out: &mut Vec<u8>,
        extended_handshake_ut_metadata: &dyn Fn() -> Option<u8>,
    ) -> anyhow::Result<usize> {
        let (lp, msg_id) = self.len_prefix_and_msg_id();

        out.resize(PREAMBLE_LEN, 0);

        byteorder::BigEndian::write_u32(&mut out[..4], lp);
        out[4] = msg_id;

        let ser = bopts();

        match self {
            Message::Request(request) | Message::Cancel(request) => {
                const MSG_LEN: usize = PREAMBLE_LEN + 12;
                out.resize(MSG_LEN, 0);
                debug_assert_eq!(out[PREAMBLE_LEN..].len(), 12);
                ser.serialize_into(&mut out[PREAMBLE_LEN..], request)
                    .unwrap();
                Ok(MSG_LEN)
            }
            Message::Bitfield(b) => {
                let block_len = b.as_ref().len();
                let msg_len = PREAMBLE_LEN + block_len;
                out.resize(msg_len, 0);
                out[PREAMBLE_LEN..PREAMBLE_LEN + block_len].copy_from_slice(b.as_ref());
                Ok(msg_len)
            }
            Message::Choke | Message::Unchoke | Message::Interested | Message::NotInterested => {
                Ok(PREAMBLE_LEN)
            }
            Message::Piece(p) => {
                let block_len = p.block.as_ref().len();
                let payload_len = 8 + block_len;
                let msg_len = PREAMBLE_LEN + payload_len;
                out.resize(msg_len, 0);
                let tmp = &mut out[PREAMBLE_LEN..];
                p.serialize(&mut tmp[..payload_len]);
                Ok(msg_len)
            }
            Message::KeepAlive => {
                // the len prefix was already written out to buf
                Ok(4)
            }
            Message::Have(v) => {
                let msg_len = PREAMBLE_LEN + 4;
                out.resize(msg_len, 0);
                BE::write_u32(&mut out[PREAMBLE_LEN..], *v);
                Ok(msg_len)
            }
            Message::Extended(e) => {
                e.serialize(out, extended_handshake_ut_metadata)?;
                let msg_size = out.len();
                // no fucking idea why +1, but I tweaked that for it all to match up
                // with real messages.
                BE::write_u32(&mut out[..4], (msg_size - PREAMBLE_LEN + 1) as u32);
                Ok(msg_size)
            }
        }
    }
    pub fn deserialize<'a>(
        buf: &'a [u8],
    ) -> Result<(Message<ByteBuf>, usize), MessageDeserializeError>
    where
        ByteBuf: From<&'a [u8]> + 'a + Deserialize<'a>,
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
                match rest.get(..expected_len) {
                    Some(h) => Ok((Message::Have(BE::read_u32(h)), PREAMBLE_LEN + expected_len)),
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
                match rest.get(..expected_len) {
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
            MSGID_REQUEST | MSGID_CANCEL => {
                let expected_len = 12;
                match rest.get(..expected_len) {
                    Some(b) => {
                        let request = decoder_config.deserialize::<Request>(b).unwrap();
                        let req = if msg_id == MSGID_REQUEST {
                            Message::Request(request)
                        } else {
                            Message::Cancel(request)
                        };
                        Ok((req, PREAMBLE_LEN + expected_len))
                    }
                    None => {
                        let missing = expected_len - rest.len();
                        Err(MessageDeserializeError::NotEnoughData(
                            missing,
                            if msg_id == MSGID_REQUEST {
                                "request"
                            } else {
                                "cancel"
                            },
                        ))
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
                        Message::Piece(Piece::deserialize(b)),
                        PREAMBLE_LEN + expected_len,
                    )),
                    None => Err(MessageDeserializeError::NotEnoughData(
                        expected_len - rest.len(),
                        "piece",
                    )),
                }
            }
            MSGID_EXTENDED => {
                if len_prefix <= 6 {
                    return Err(MessageDeserializeError::IncorrectLenPrefix {
                        expected: 6,
                        received: len_prefix,
                        msg_id,
                    });
                }
                // TODO: NO clue why - 1 here. Empirically figured out.
                let expected_len = len_prefix as usize - 1;
                match rest.get(..expected_len) {
                    Some(b) => Ok((
                        Message::Extended(ExtendedMessage::deserialize(b)?),
                        PREAMBLE_LEN + expected_len,
                    )),
                    None => Err(MessageDeserializeError::NotEnoughData(
                        expected_len - rest.len(),
                        "extended",
                    )),
                }
            }
            msg_id => Err(MessageDeserializeError::UnsupportedMessageId(msg_id)),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Handshake<ByteBuf> {
    pub pstr: ByteBuf,
    pub reserved: [u8; 8],
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
}

fn bopts() -> impl bincode::Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_big_endian()
}

impl Handshake<ByteBuf<'static>> {
    pub fn new(info_hash: Id20, peer_id: Id20) -> Handshake<ByteBuf<'static>> {
        debug_assert_eq!(PSTR_BT1.len(), 19);

        let mut reserved: u64 = 0;
        // supports extended messaging
        reserved |= 1 << 20;
        let mut reserved_arr = [0u8; 8];
        BE::write_u64(&mut reserved_arr, reserved);

        Handshake {
            pstr: ByteBuf(PSTR_BT1.as_bytes()),
            reserved: reserved_arr,
            info_hash: info_hash.0,
            peer_id: peer_id.0,
        }
    }

    pub fn deserialize(
        b: &[u8],
    ) -> Result<(Handshake<ByteBuf<'_>>, usize), MessageDeserializeError> {
        let pstr_len = *b
            .first()
            .ok_or(MessageDeserializeError::NotEnoughData(1, "handshake"))?;
        if pstr_len as usize != PSTR_BT1.len() {
            return Err(MessageDeserializeError::Other(anyhow::anyhow!(
                "pstr should be {} bytes long, but received {}",
                PSTR_BT1.len(),
                pstr_len
            )));
        }
        let expected_len = 1usize + pstr_len as usize + 48;
        let hbuf = b
            .get(..expected_len)
            .ok_or(MessageDeserializeError::NotEnoughData(
                expected_len,
                "handshake",
            ))?;
        let h = Self::bopts()
            .deserialize::<Handshake<ByteBuf<'_>>>(hbuf)
            .map_err(|e| MessageDeserializeError::Other(e.into()))?;
        if h.pstr.0 != PSTR_BT1.as_bytes() {
            return Err(MessageDeserializeError::Other(anyhow::anyhow!(
                "pstr doesn't match bittorrent V1"
            )));
        }
        Ok((h, expected_len))
    }
}

impl<B> Handshake<B> {
    pub fn supports_extended(&self) -> bool {
        self.reserved[5] & 0x10 > 0
    }
    fn bopts() -> impl bincode::Options {
        bincode::DefaultOptions::new()
    }

    pub fn serialize(&self, buf: &mut Vec<u8>)
    where
        B: Serialize,
    {
        Self::bopts().serialize_into(buf, &self).unwrap()
    }
}

impl<B> CloneToOwned for Handshake<B>
where
    B: CloneToOwned,
{
    type Target = Handshake<<B as CloneToOwned>::Target>;

    fn clone_to_owned(&self) -> Self::Target {
        Handshake {
            pstr: self.pstr.clone_to_owned(),
            reserved: self.reserved,
            info_hash: self.info_hash,
            peer_id: self.peer_id,
        }
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
    use crate::extended::handshake::ExtendedHandshake;

    use super::*;
    #[test]
    fn test_handshake_serialize() {
        let info_hash = Id20::new([
            1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ]);
        let peer_id = Id20::new([
            1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
        ]);
        let mut buf = Vec::new();
        Handshake::new(info_hash, peer_id).serialize(&mut buf);
        assert_eq!(buf.len(), 20 + 20 + 8 + 19 + 1);
    }

    #[test]
    fn test_extended_serialize() {
        let msg = Message::Extended(ExtendedMessage::Handshake(ExtendedHandshake::new()));
        let mut out = Vec::new();
        msg.serialize(&mut out, &|| None).unwrap();
        dbg!(out);
    }

    #[test]
    fn test_deserialize_serialize_extended_is_same() {
        use std::fs::File;
        use std::io::Read;
        let mut buf = Vec::new();
        File::open("../librqbit/resources/test/extended-handshake.bin")
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();
        let (msg, size) = MessageBorrowed::deserialize(&buf).unwrap();
        assert_eq!(size, buf.len());
        let mut write_buf = Vec::new();
        msg.serialize(&mut write_buf, &|| None).unwrap();
        if buf != write_buf {
            {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .open("/tmp/test_deserialize_serialize_extended_is_same")
                    .unwrap();
                f.write_all(&write_buf).unwrap();
            }
            panic!("resources/test/extended-handshake.bin did not serialize exactly the same. Dumped to /tmp/test_deserialize_serialize_extended_is_same, you can compare with resources/test/extended-handshake.bin")
        }
    }
}
