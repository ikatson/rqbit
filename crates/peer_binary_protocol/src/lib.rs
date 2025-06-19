// BitTorrent peer protocol implementation: parsing, serialization etc.
//
// Can be used outside of librqbit.

mod double_buf;
pub mod extended;

use std::io::IoSlice;

use bincode::Options;
use buffers::{ByteBuf, ByteBufOwned, ByteBufT};
use byteorder::{BE, ByteOrder};
use bytes::Bytes;
use clone_to_owned::CloneToOwned;
use extended::PeerExtendedMessageIds;
use librqbit_core::{constants::CHUNK_SIZE, hash_id::Id20, lengths::ChunkInfo};
use serde::{Deserialize, Serialize};

use crate::double_buf::DoubleBufHelper;

use self::extended::ExtendedMessage;

const INTEGER_LEN: usize = 4;
const MSGID_LEN: usize = 1;
const PREAMBLE_LEN: usize = INTEGER_LEN + MSGID_LEN;
const PIECE_MESSAGE_PREAMBLE_LEN: usize = PREAMBLE_LEN + INTEGER_LEN * 2;
pub const PIECE_MESSAGE_DEFAULT_LEN: usize = PIECE_MESSAGE_PREAMBLE_LEN + CHUNK_SIZE as usize;

const NO_PAYLOAD_MSG_LEN: usize = PREAMBLE_LEN;

const PSTR_BT1: &str = "BitTorrent protocol";

type MsgId = u8;

const LEN_PREFIX_KEEPALIVE: u32 = 0;
const LEN_PREFIX_CHOKE: u32 = MSGID_LEN as u32;
const LEN_PREFIX_UNCHOKE: u32 = MSGID_LEN as u32;
const LEN_PREFIX_INTERESTED: u32 = MSGID_LEN as u32;
const LEN_PREFIX_NOT_INTERESTED: u32 = MSGID_LEN as u32;
const LEN_PREFIX_HAVE: u32 = MSGID_LEN as u32 + INTEGER_LEN as u32;
const LEN_PREFIX_PIECE: u32 = MSGID_LEN as u32 + INTEGER_LEN as u32 * 2;
const LEN_PREFIX_REQUEST: u32 = MSGID_LEN as u32 + INTEGER_LEN as u32 * 3;

const MSGID_CHOKE: MsgId = 0;
const MSGID_UNCHOKE: MsgId = 1;
const MSGID_INTERESTED: MsgId = 2;
const MSGID_NOT_INTERESTED: MsgId = 3;
const MSGID_HAVE: MsgId = 4;
const MSGID_BITFIELD: MsgId = 5;
const MSGID_REQUEST: MsgId = 6;
const MSGID_PIECE: MsgId = 7;
const MSGID_CANCEL: MsgId = 8;
const MSGID_EXTENDED: MsgId = 20;

pub const EXTENDED_UT_METADATA_KEY: &[u8] = b"ut_metadata";
pub const MY_EXTENDED_UT_METADATA: u8 = 3;

pub const EXTENDED_UT_PEX_KEY: &[u8] = b"ut_pex";
pub const MY_EXTENDED_UT_PEX: u8 = 1;

#[derive(thiserror::Error, Debug)]
pub enum MessageDeserializeError {
    #[error("not enough data to deserialize {2} (msgid={1:?}): expected at least {0} more bytes")]
    NotEnoughData(usize, Option<MsgId>, &'static str),
    #[error("need a contiguous input to deserialize")]
    NeedContiguous,
    #[error("unsupported message id {0}")]
    UnsupportedMessageId(u8),
    #[error(transparent)]
    Bencode(#[from] bencode::DeserializeError),
    #[error(transparent)]
    Bincode(#[from] bincode::Error),
    #[error("error deserializing {name}: {error:#}")]
    BincodeWithName {
        #[source]
        error: bincode::Error,
        name: &'static str,
    },
    #[error(
        "incorrect len prefix for message id {msg_id}, expected {expected}, received {received}"
    )]
    IncorrectLenPrefix {
        received: u32,
        expected: u32,
        msg_id: u8,
    },
    #[error("{0}")]
    Text(&'static str),
    #[error("unrecognized ut_metadata message type: {0}")]
    UnrecognizedUtMetadata(u32),
    #[error("pstr should be 19 bytes long but got {0}")]
    InvalidPstr(u8),
}

pub fn serialize_piece_preamble(chunk: &ChunkInfo, mut buf: &mut [u8]) -> usize {
    BE::write_u32(&mut buf[0..4], LEN_PREFIX_PIECE + chunk.size);
    buf[4] = MSGID_PIECE;

    buf = &mut buf[PREAMBLE_LEN..];
    BE::write_u32(&mut buf[0..4], chunk.piece_index.get());
    BE::write_u32(&mut buf[4..8], chunk.offset);

    PIECE_MESSAGE_PREAMBLE_LEN
}

pub struct Piece<B> {
    pub index: u32,
    pub begin: u32,
    block_0: B,
    block_1: B,
}

impl<B: AsRef<[u8]>> std::fmt::Debug for Piece<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Piece")
            .field("index", &self.index)
            .field("begin", &self.begin)
            .field("len", &self.block_0.as_ref().len())
            .finish()
    }
}

impl<B: CloneToOwned> CloneToOwned for Piece<B> {
    type Target = Piece<B::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        Piece {
            index: self.index,
            begin: self.begin,
            block_0: self.block_0.clone_to_owned(within_buffer),
            block_1: self.block_1.clone_to_owned(within_buffer),
        }
    }
}

impl<B> Piece<B>
where
    B: ByteBufT,
{
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.block_0.as_ref().len() + self.block_1.as_ref().len()
    }

    pub fn from_data<T>(index: u32, begin: u32, block: T) -> Piece<B>
    where
        B: From<T>,
        T: Default,
    {
        Piece {
            index,
            begin,
            block_0: B::from(block),
            block_1: B::from(T::default()),
        }
    }

    pub fn data(&self) -> (&[u8], &[u8]) {
        (self.block_0.as_slice(), self.block_1.as_slice())
    }

    pub fn as_ioslices(&self) -> [IoSlice<'_>; 2] {
        [
            IoSlice::new(self.block_0.as_slice()),
            IoSlice::new(self.block_0.as_slice()),
        ]
    }

    pub fn serialize(&self, mut buf: &mut [u8]) -> usize {
        byteorder::BigEndian::write_u32(&mut buf[0..4], self.index);
        byteorder::BigEndian::write_u32(&mut buf[4..8], self.begin);
        buf = &mut buf[8..];

        let b0 = self.block_0.as_ref();
        let b1 = self.block_0.as_ref();

        buf[..b0.len()].copy_from_slice(b0);
        buf = &mut buf[b0.len()..];
        buf[..b1.len()].copy_from_slice(b1);
        8 + b0.len() + b1.len()
    }
}

#[derive(Debug)]
pub enum Message<ByteBuf: ByteBufT> {
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
pub type MessageOwned = Message<ByteBufOwned>;

pub type BitfieldBorrowed<'a> = &'a bitvec::slice::BitSlice<u8, bitvec::order::Msb0>;
pub type BitfieldOwned = bitvec::vec::BitVec<u8, bitvec::order::Msb0>;

pub struct Bitfield<'a> {
    pub data: BitfieldBorrowed<'a>,
}

impl<ByteBuf> CloneToOwned for Message<ByteBuf>
where
    ByteBuf: ByteBufT,
    <ByteBuf as CloneToOwned>::Target: ByteBufT,
{
    type Target = Message<<ByteBuf as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        match self {
            Message::Request(req) => Message::Request(*req),
            Message::Cancel(req) => Message::Cancel(*req),
            Message::Bitfield(b) => Message::Bitfield(b.clone_to_owned(within_buffer)),
            Message::Choke => Message::Choke,
            Message::Unchoke => Message::Unchoke,
            Message::Interested => Message::Interested,
            Message::Piece(piece) => Message::Piece(piece.clone_to_owned(within_buffer)),
            Message::KeepAlive => Message::KeepAlive,
            Message::Have(v) => Message::Have(*v),
            Message::NotInterested => Message::NotInterested,
            Message::Extended(e) => Message::Extended(e.clone_to_owned(within_buffer)),
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

impl std::fmt::Debug for Bitfield<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bitfield")
            .field("_ones", &self.data.count_ones())
            .field("_len", &self.data.len())
            .finish()
    }
}

impl<ByteBuf> Message<ByteBuf>
where
    ByteBuf: ByteBufT,
{
    pub fn len_prefix_and_msg_id(&self) -> (u32, u8) {
        match self {
            Message::Request(_) => (LEN_PREFIX_REQUEST, MSGID_REQUEST),
            Message::Cancel(_) => (LEN_PREFIX_REQUEST, MSGID_CANCEL),
            Message::Bitfield(b) => (MSGID_LEN as u32 + b.as_ref().len() as u32, MSGID_BITFIELD),
            Message::Choke => (LEN_PREFIX_CHOKE, MSGID_CHOKE),
            Message::Unchoke => (LEN_PREFIX_UNCHOKE, MSGID_UNCHOKE),
            Message::Interested => (LEN_PREFIX_INTERESTED, MSGID_INTERESTED),
            Message::NotInterested => (LEN_PREFIX_NOT_INTERESTED, MSGID_NOT_INTERESTED),
            Message::Piece(p) => (LEN_PREFIX_PIECE + p.len() as u32, MSGID_PIECE),
            Message::KeepAlive => (LEN_PREFIX_KEEPALIVE, 0),
            Message::Have(_) => (LEN_PREFIX_HAVE, MSGID_HAVE),
            Message::Extended(_) => (0, MSGID_EXTENDED),
        }
    }
    pub fn serialize(
        &self,
        out: &mut Vec<u8>,
        peer_extended_messages: &dyn Fn() -> PeerExtendedMessageIds,
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
                let block_len = p.len();
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
                e.serialize(out, peer_extended_messages)?;
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
        buf2: &'a [u8],
    ) -> Result<(Message<ByteBuf>, usize), MessageDeserializeError>
    where
        ByteBuf: From<&'a [u8]> + 'a + Deserialize<'a>,
    {
        let mut buf = DoubleBufHelper::new(buf, buf2);
        let len_prefix = buf.read_u32_be().map_err(|rem| {
            MessageDeserializeError::NotEnoughData(rem, None, "message len_prefix")
        })?;
        if len_prefix == 0 {
            return Ok((Message::KeepAlive, 4));
        }

        let msg_id = buf
            .read_u8()
            .map_err(|rem| MessageDeserializeError::NotEnoughData(rem, None, "message msg_id"))?;

        let msg_len = len_prefix as usize - MSGID_LEN;

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
                let have = buf
                    .read_u32_be()
                    .map_err(|e| MessageDeserializeError::NotEnoughData(e, Some(msg_id), "have"))?;
                Ok((Message::Have(have), PREAMBLE_LEN + INTEGER_LEN))
            }
            MSGID_BITFIELD => {
                if len_prefix <= 1 {
                    return Err(MessageDeserializeError::IncorrectLenPrefix {
                        expected: 2,
                        received: len_prefix,
                        msg_id,
                    });
                }
                if buf.len() < msg_len {
                    return Err(MessageDeserializeError::NotEnoughData(
                        msg_len - buf.len(),
                        Some(msg_id),
                        "bitfield",
                    ));
                }
                let data = buf
                    .get_contiguous(msg_len)
                    .ok_or(MessageDeserializeError::NeedContiguous)?;
                Ok((
                    Message::Bitfield(ByteBuf::from(data)),
                    PREAMBLE_LEN + msg_len,
                ))
            }
            MSGID_REQUEST | MSGID_CANCEL => {
                const I32: usize = 4;
                const I32_3: usize = I32 * 3;
                let req = buf.consume::<I32_3>().map_err(|missing| {
                    MessageDeserializeError::NotEnoughData(
                        missing,
                        Some(msg_id),
                        if msg_id == MSGID_REQUEST {
                            "request"
                        } else {
                            "cancel"
                        },
                    )
                })?;
                let request = Request {
                    index: BE::read_u32(&req[0..I32]),
                    begin: BE::read_u32(&req[I32..I32 * 2]),
                    length: BE::read_u32(&req[I32 * 2..I32 * 3]),
                };
                let req = if msg_id == MSGID_REQUEST {
                    Message::Request(request)
                } else {
                    Message::Cancel(request)
                };
                Ok((req, PREAMBLE_LEN + I32_3))
            }
            MSGID_PIECE => {
                const MIN_PAYLOAD: usize = 1;
                const MIN_LENGTH: usize = INTEGER_LEN * 2 + MIN_PAYLOAD;
                if msg_len < MIN_LENGTH {
                    return Err(MessageDeserializeError::IncorrectLenPrefix {
                        expected: (MIN_LENGTH + MSGID_LEN) as u32,
                        received: len_prefix,
                        msg_id,
                    });
                }

                let index = buf.read_u32_be().map_err(|missing| {
                    MessageDeserializeError::NotEnoughData(missing, Some(msg_id), "piece index")
                })?;
                let begin = buf.read_u32_be().map_err(|missing| {
                    MessageDeserializeError::NotEnoughData(missing, Some(msg_id), "piece begin")
                })?;

                let block_len = msg_len - INTEGER_LEN * 2;

                let (block_0, block_1) = buf.consume_variable(block_len).map_err(|missing| {
                    MessageDeserializeError::NotEnoughData(missing, Some(msg_id), "piece data")
                })?;

                Ok((
                    Message::Piece(Piece {
                        index,
                        begin,
                        block_0: block_0.into(),
                        block_1: block_1.into(),
                    }),
                    PREAMBLE_LEN + len_prefix as usize - MSGID_LEN,
                ))
            }
            MSGID_EXTENDED => {
                if len_prefix <= 6 {
                    return Err(MessageDeserializeError::IncorrectLenPrefix {
                        expected: 6,
                        received: len_prefix,
                        msg_id,
                    });
                }

                if buf.len() < msg_len {
                    return Err(MessageDeserializeError::NotEnoughData(
                        msg_len - buf.len(),
                        Some(msg_id),
                        "extended",
                    ));
                }

                let msg_data = buf
                    .get_contiguous(msg_len)
                    .ok_or(MessageDeserializeError::NeedContiguous)?;

                Ok((
                    Message::Extended(ExtendedMessage::deserialize(msg_data)?),
                    PREAMBLE_LEN + msg_len,
                ))
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
        let pstr_len =
            *b.first()
                .ok_or(MessageDeserializeError::NotEnoughData(1, None, "handshake"))?;
        if pstr_len as usize != PSTR_BT1.len() {
            return Err(MessageDeserializeError::InvalidPstr(pstr_len));
        }
        let expected_len = 1usize + pstr_len as usize + 48;
        let hbuf = b
            .get(..expected_len)
            .ok_or(MessageDeserializeError::NotEnoughData(
                expected_len,
                None,
                "handshake",
            ))?;
        let h = Self::bopts().deserialize::<Handshake<ByteBuf<'_>>>(hbuf)?;
        if h.pstr.0 != PSTR_BT1.as_bytes() {
            return Err(MessageDeserializeError::Text(
                "pstr doesn't match bittorrent V1",
            ));
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

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        Handshake {
            pstr: self.pstr.clone_to_owned(within_buffer),
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
        msg.serialize(&mut out, &Default::default).unwrap();
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
        let (msg, size) = MessageBorrowed::deserialize(&buf, &[]).unwrap();
        assert_eq!(size, buf.len());
        let mut write_buf = Vec::new();
        msg.serialize(&mut write_buf, &Default::default).unwrap();
        if buf != write_buf {
            {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open("/tmp/test_deserialize_serialize_extended_is_same")
                    .unwrap();
                f.write_all(&write_buf).unwrap();
            }
            panic!(
                "resources/test/extended-handshake.bin did not serialize exactly the same. Dumped to /tmp/test_deserialize_serialize_extended_is_same, you can compare with resources/test/extended-handshake.bin"
            )
        }
    }

    #[test]
    fn test_deserialize_piece() {
        const LEN: usize = 100;
        const EXTRA: usize = 100;
        let mut buf = [0u8; LEN + EXTRA];

        let block_len = LEN - PREAMBLE_LEN - INTEGER_LEN * 2;
        let len_prefix: u32 = (block_len + INTEGER_LEN * 2 + MSGID_LEN) as u32;
        let index: u32 = 42;
        let begin: u32 = 43;

        buf[0..4].copy_from_slice(&len_prefix.to_be_bytes());
        buf[4] = MSGID_PIECE;
        buf[5..9].copy_from_slice(&index.to_be_bytes());
        buf[9..13].copy_from_slice(&begin.to_be_bytes());

        for split_point in 0..buf.len() {
            dbg!(split_point);
            let (first, second) = buf.split_at(split_point);
            let (msg, len) = MessageBorrowed::deserialize(first, second).unwrap();

            let piece = match msg {
                Message::Piece(piece) => piece,
                other => panic!("expected piece got {other:?}"),
            };

            assert_eq!(piece.len(), block_len);
            assert_eq!(piece.index, index);
            assert_eq!(piece.begin, begin);
            assert_eq!(len, LEN);
        }
    }
}
