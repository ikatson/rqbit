use std::collections::HashMap;

use bincode::Options;
use byteorder::{ByteOrder, BE};
use serde::{Deserialize, Serialize};

use crate::{
    bencode_value::BencodeValue,
    buffers::{ByteBuf, ByteString},
    clone_to_owned::CloneToOwned,
    constants::CHUNK_SIZE,
    lengths::ChunkInfo,
};

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
const MSGID_EXTENDED: u8 = 20;

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
            MessageDeserializeError::Other(e) => write!(f, "{}", e),
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

pub type BitfieldBorrowed<'a> = &'a bitvec::slice::BitSlice<bitvec::order::Lsb0, u8>;
pub type BitfieldOwned = bitvec::vec::BitVec<bitvec::order::Lsb0, u8>;

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
            Message::Extended(_) => unimplemented!(),
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
    ByteBuf: AsRef<[u8]> + std::hash::Hash + Eq + Serialize,
{
    pub fn len_prefix_and_msg_id(&self) -> (u32, u8) {
        match self {
            Message::Request(_) => (LEN_PREFIX_REQUEST, MSGID_REQUEST),
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
            Message::Bitfield(b) => {
                let block_len = b.as_ref().len();
                let msg_len = PREAMBLE_LEN + block_len;
                out.resize(msg_len, 0);
                (&mut out[PREAMBLE_LEN..PREAMBLE_LEN + block_len]).copy_from_slice(b.as_ref());
                msg_len
            }
            Message::Choke | Message::Unchoke | Message::Interested | Message::NotInterested => {
                PREAMBLE_LEN
            }
            Message::Piece(p) => {
                let block_len = p.block.as_ref().len();
                let payload_len = 8 + block_len;
                let msg_len = PREAMBLE_LEN + payload_len;
                out.resize(msg_len, 0);
                let tmp = &mut out[PREAMBLE_LEN..];
                p.serialize(&mut tmp[..payload_len]);
                msg_len
            }
            Message::KeepAlive => {
                // the len prefix was already written out to buf
                4
            }
            Message::Have(v) => {
                let msg_len = PREAMBLE_LEN + 4;
                out.resize(msg_len, 0);
                BE::write_u32(&mut out[PREAMBLE_LEN..], *v);
                msg_len
            }
            Message::Extended(e) => {
                e.serialize(out);
                let msg_size = out.len();
                BE::write_u32(&mut out[..4], msg_size as u32);
                msg_size
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
                match rest.get(..expected_len as usize) {
                    Some(h) => Ok((Message::Have(BE::read_u32(&h)), PREAMBLE_LEN + expected_len)),
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
                        Message::Extended(ExtendedMessage::deserialize(&b)?),
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

        let mut reserved: u64 = 0;
        // supports extended messaging
        reserved |= 1 << 20;
        let mut reserved_arr = [0u8; 8];
        BE::write_u64(&mut reserved_arr, reserved);

        Handshake {
            pstr: PSTR_BT1,
            reserved: reserved_arr,
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

#[derive(Debug)]
pub enum ExtendedMessage<ByteBuf: std::hash::Hash + Eq> {
    Handshake(ExtendedHandshake<ByteBuf>),
    Dyn(u8, BencodeValue<ByteBuf>),
}

impl<ByteBuf: std::hash::Hash + Eq + Serialize> ExtendedMessage<ByteBuf> {
    fn serialize(&self, out: &mut Vec<u8>) {
        match self {
            ExtendedMessage::Dyn(msg_id, v) => {
                out.push(*msg_id);
                crate::serde_bencode_ser::bencode_serialize_to_writer(v, out).unwrap()
            }
            ExtendedMessage::Handshake(h) => {
                out.push(0);
                crate::serde_bencode_ser::bencode_serialize_to_writer(h, out).unwrap()
            }
        }
    }

    fn deserialize<'de>(mut buf: &'de [u8]) -> Result<Self, MessageDeserializeError>
    where
        ByteBuf: Deserialize<'de> + From<&'de [u8]>,
    {
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open("/tmp/msg")
                .unwrap();
            f.write_all(buf).unwrap();
        }

        use crate::serde_bencode_de::from_bytes;

        let emsg_id = buf.get(0).copied().ok_or_else(|| {
            MessageDeserializeError::Other(anyhow::anyhow!(
                "cannot deserialize extended message: can't read first byte"
            ))
        })?;

        buf = &buf.get(1..).ok_or_else(|| {
            MessageDeserializeError::Other(anyhow::anyhow!(
                "cannot deserialize extended message: buffer empty"
            ))
        })?;

        match emsg_id {
            // handshake
            0 => Ok(ExtendedMessage::Handshake(from_bytes(&buf)?)),
            other => Ok(ExtendedMessage::Dyn(other, from_bytes(&buf)?)),
        }
        // match self {
        //     ExtendedMessage::Dyn(v, msg) => {
        //         crate::bencode_value::dyn_from_bytes(buf)
        //     }
        //     ExtendedMessage::Handshake(h) => {
        //         crate::serde_bencode_ser::bencode_serialize_to_writer(h, out).unwrap()
        //     }
        // }
    }
}

#[derive(Deserialize, Serialize, Debug)]
pub struct ExtendedHandshake<ByteBuf: Eq + std::hash::Hash> {
    #[serde(bound(deserialize = "ByteBuf: From<&'de [u8]>"))]
    pub m: HashMap<ByteBuf, BencodeValue<ByteBuf>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub v: Option<ByteBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yourip: Option<ByteBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv6: Option<ByteBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ipv4: Option<ByteBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reqq: Option<u32>,
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, str::FromStr};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::peer_id::generate_peer_id;

    fn decode_info_hash(hash_str: &str) -> [u8; 20] {
        let mut hash_arr = [0u8; 20];
        hex::decode_to_slice(hash_str, &mut hash_arr).unwrap();
        hash_arr
    }

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

    #[test]
    fn test_extended_serialize() {
        let mut feats = HashMap::new();
        feats.insert("whatever".as_bytes().into(), BencodeValue::Integer(1));

        let msg =
            Message::<ByteBuf<'static>>::Extended(ExtendedMessage::Handshake(ExtendedHandshake {
                m: feats,
                p: None,
                v: None,
                yourip: None,
                ipv6: None,
                ipv4: None,
                reqq: None,
            }));

        let mut out = Vec::new();
        msg.serialize(&mut out);
        dbg!(out);
    }

    #[tokio::test]
    async fn test_connect_to_local_qbittorrent() {
        let mut stream =
            tokio::net::TcpStream::connect(SocketAddr::from_str("127.0.0.1:27311").unwrap())
                .await
                .unwrap();
        let peer_id = generate_peer_id();
        let info_hash = decode_info_hash("9905f844e5d8787ecd5e08fb46b2eb0a42c131d7");
        dbg!(info_hash);
        let handshake = dbg!(Handshake::new(info_hash, peer_id));

        let mut write_buf = Vec::<u8>::new();
        let h = handshake.serialize();

        let mut read_buf = vec![0u8; 16384];

        stream.write_all(&h).await.unwrap();

        let read_bytes = stream.read(&mut read_buf).await.unwrap();
        let (handshake, hlen) = Handshake::deserialize(&read_buf[..read_bytes]).unwrap();
        dbg!(handshake);

        read_buf.copy_within(hlen..read_bytes, 0);
        let mut read_so_far = read_bytes - hlen;

        loop {
            let (message, size) = loop {
                match MessageBorrowed::deserialize(&read_buf[..read_so_far]) {
                    Ok((msg, size)) => {
                        break (msg, size);
                    }
                    Err(MessageDeserializeError::NotEnoughData(d, _)) => {
                        if read_buf.len() < read_so_far + d {
                            read_buf.reserve(d);
                            read_buf.resize(read_buf.capacity(), 0);
                        }

                        let size = stream.read(&mut read_buf[read_so_far..]).await.unwrap();
                        if size == 0 {
                            panic!("size == 0, disconnected")
                        }
                        read_so_far += size;
                    }
                    Err(e) => Err(e).unwrap(),
                }
            };

            dbg!(message, size);

            if read_so_far > size {
                read_buf.copy_within(size..read_so_far, 0);
            }
            read_so_far -= size;
        }
    }
}
