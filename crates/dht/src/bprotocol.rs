use std::{
    io::Write,
    marker::PhantomData,
    net::{SocketAddr, SocketAddrV4, SocketAddrV6},
};

use bencode::{ByteBuf, ByteBufOwned};
use buffers::ByteBufT;
use bytes::Bytes;
use clone_to_owned::CloneToOwned;
use librqbit_core::{
    compact_ip::{
        Compact, CompactListInBuffer, CompactSerialize, CompactSerializeFixedLen, CompactSocketAddr,
    },
    hash_id::Id20,
};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{IgnoredAny, Unexpected},
};

#[derive(Debug)]
enum MessageType {
    Request,
    Response,
    Error,
}

impl<'de> Deserialize<'de> for MessageType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = MessageType;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(formatter, r#""q", "e" or "r" bencode string"#)
            }
            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let msg = match v {
                    b"q" => MessageType::Request,
                    b"r" => MessageType::Response,
                    b"e" => MessageType::Error,
                    _ => return Err(E::invalid_value(Unexpected::Bytes(v), &self)),
                };
                Ok(msg)
            }
        }
        deserializer.deserialize_bytes(Visitor {})
    }
}

impl Serialize for MessageType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            MessageType::Request => serializer.serialize_bytes(b"q"),
            MessageType::Response => serializer.serialize_bytes(b"r"),
            MessageType::Error => serializer.serialize_bytes(b"e"),
        }
    }
}

#[derive(Debug)]
pub struct ErrorDescription<BufT> {
    pub code: i32,
    pub description: BufT,
}

impl<BufT> CloneToOwned for ErrorDescription<BufT>
where
    BufT: CloneToOwned,
{
    type Target = ErrorDescription<<BufT as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        ErrorDescription {
            code: self.code,
            description: self.description.clone_to_owned(within_buffer),
        }
    }
}

impl<BufT> Serialize for ErrorDescription<BufT>
where
    BufT: Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(2))?;
        seq.serialize_element(&self.code)?;
        seq.serialize_element(&self.description)?;
        seq.end()
    }
}

impl<'de, BufT> Deserialize<'de> for ErrorDescription<BufT>
where
    BufT: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor<BufT> {
            phantom: PhantomData<BufT>,
        }
        impl<'de, BufT> serde::de::Visitor<'de> for Visitor<BufT>
        where
            BufT: Deserialize<'de>,
        {
            type Value = ErrorDescription<BufT>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(formatter, r#"a list [i32, string]"#)
            }
            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                use serde::de::Error;
                let code = match seq.next_element::<i32>()? {
                    Some(code) => code,
                    None => return Err(A::Error::invalid_length(0, &self)),
                };
                let description = match seq.next_element::<BufT>()? {
                    Some(code) => code,
                    None => return Err(A::Error::invalid_length(1, &self)),
                };
                // The type doesn't matter here, we are just making sure the list is over.
                if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(A::Error::invalid_length(3, &self));
                }
                Ok(ErrorDescription { code, description })
            }
        }
        deserializer.deserialize_seq(Visitor {
            phantom: PhantomData,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct RawMessage<BufT, Args = IgnoredAny, Resp = IgnoredAny> {
    #[serde(rename = "y")]
    message_type: MessageType,
    #[serde(rename = "t")]
    transaction_id: BufT,
    #[serde(rename = "e", skip_serializing_if = "Option::is_none")]
    error: Option<ErrorDescription<BufT>>,
    #[serde(rename = "r", skip_serializing_if = "Option::is_none")]
    response: Option<Resp>,
    #[serde(rename = "q", skip_serializing_if = "Option::is_none")]
    method_name: Option<BufT>,
    #[serde(rename = "a", skip_serializing_if = "Option::is_none")]
    arguments: Option<Args>,
    #[serde(rename = "v", skip_serializing_if = "Option::is_none")]
    version: Option<BufT>,
    #[serde(rename = "ip", skip_serializing_if = "Option::is_none")]
    ip: Option<CompactSocketAddr>,
}

pub struct Node<A> {
    pub id: Id20,
    pub addr: A,
}

impl<A: Into<SocketAddr> + Copy> Node<A> {
    pub fn as_socketaddr(&self) -> Node<SocketAddr> {
        Node {
            id: self.id,
            addr: self.addr.into(),
        }
    }
}

pub type CompactNodeInfo<Buf, A> = CompactListInBuffer<Buf, Node<A>>;
pub type CompactNodeInfoOwned<A> = CompactNodeInfo<ByteBufOwned, A>;

impl CompactSerialize for Node<SocketAddrV4> {
    type Slice = [u8; 26];

    fn expecting() -> &'static str {
        "26 bytes"
    }

    fn as_slice(&self) -> Self::Slice {
        let mut data = [0u8; 26];
        data[..20].copy_from_slice(&self.id.0);
        data[20..26].copy_from_slice(self.addr.as_slice().as_ref());
        data
    }

    fn from_slice(buf: &[u8]) -> Option<Self> {
        if buf.len() != 26 {
            return None;
        }
        Some(Self::from_slice_unchecked_len(buf))
    }

    fn from_slice_unchecked_len(buf: &[u8]) -> Self {
        Node {
            id: Id20::from_bytes(&buf[..20]).unwrap(),
            addr: SocketAddrV4::from_slice_unchecked_len(&buf[20..26]),
        }
    }
}

impl<A: CompactSerializeFixedLen> CompactSerializeFixedLen for Node<A> {
    fn fixed_len() -> usize {
        20 + A::fixed_len()
    }
}

impl CompactSerialize for Node<SocketAddrV6> {
    type Slice = [u8; 38];

    fn expecting() -> &'static str {
        "38 bytes"
    }

    fn as_slice(&self) -> Self::Slice {
        let mut data = [0u8; 38];
        data[..20].copy_from_slice(&self.id.0);
        data[20..38].copy_from_slice(self.addr.as_slice().as_ref());
        data
    }

    fn from_slice(buf: &[u8]) -> Option<Self> {
        if buf.len() != 38 {
            return None;
        }
        Some(Self::from_slice_unchecked_len(buf))
    }

    fn from_slice_unchecked_len(buf: &[u8]) -> Self {
        Node {
            id: Id20::from_bytes(&buf[..20]).unwrap(),
            addr: SocketAddrV6::from_slice_unchecked_len(&buf[20..38]),
        }
    }
}

impl<A: core::fmt::Debug> core::fmt::Debug for Node<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}={:?}", self.addr, self.id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    V4,
    V6,
    Both,
    None,
}

impl Serialize for Want {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Want::V4 => ["n4"][..].serialize(serializer),
            Want::V6 => ["n6"][..].serialize(serializer),
            Want::Both => ["n4", "n6"][..].serialize(serializer),
            Want::None => {
                const EMPTY: [&str; 0] = [];
                EMPTY[..].serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for Want {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Want;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, r#"a list with "n4", "n6" or both"#)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut want_v4 = false;
                let mut want_v6 = false;
                while let Some(item) = seq.next_element::<&[u8]>()? {
                    match item {
                        b"n4" => want_v4 = true,
                        b"n6" => want_v6 = true,
                        _ => continue,
                    }
                }
                match (want_v4, want_v6) {
                    (true, true) => Ok(Want::Both),
                    (true, false) => Ok(Want::V4),
                    (false, true) => Ok(Want::V6),
                    (false, false) => Ok(Want::None),
                }
            }
        }
        deserializer.deserialize_seq(V)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FindNodeRequest {
    pub id: Id20,
    pub target: Id20,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub want: Option<Want>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Response<BufT: ByteBufT> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<CompactSocketAddr>>,
    pub id: Id20,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nodes: Option<CompactNodeInfo<BufT, SocketAddrV4>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nodes6: Option<CompactNodeInfo<BufT, SocketAddrV6>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<BufT>,
    /// BEP 44 `get`: the stored value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub v: Option<RawBencode<BufT>>,
    /// BEP 44 `get` of a mutable item: the public key of its owner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub k: Option<FixedBytes<32>>,
    /// BEP 44 `get` of a mutable item: the ed25519 signature over salt, `seq` and `v`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sig: Option<FixedBytes<64>>,
    /// BEP 44 `get` of a mutable item: its sequence number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GetPeersRequest {
    pub id: Id20,
    pub info_hash: Id20,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub want: Option<Want>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PingRequest {
    pub id: Id20,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AnnouncePeer<BufT> {
    pub id: Id20,
    pub implied_port: u8,
    pub info_hash: Id20,
    pub port: u16,
    pub token: BufT,
}

/// A bencoded value kept as the exact bytes it came in (or was created) as: the BEP 44 `v`.
/// The target of an immutable item is the SHA-1 of these bytes and the signature of a mutable
/// item covers them, so they are never re-serialized from a parsed value.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct RawBencode<BufT>(pub BufT);

impl<BufT: AsRef<[u8]>> Serialize for RawBencode<BufT> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        bencode::raw_value::RawValue(self.0.as_ref()).serialize(serializer)
    }
}

impl<'de, BufT: Deserialize<'de>> Deserialize<'de> for RawBencode<BufT> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // The value itself is only walked (and so checked to be valid bencode); what we keep
        // are the bytes it spans.
        let raw = bencode::WithRawBytes::<IgnoredAny, BufT>::deserialize(deserializer)?;
        Ok(RawBencode(raw.raw_bytes))
    }
}

/// A byte string of exactly `N` bytes (BEP 44 `k` and `sig`). Any other length fails to decode.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FixedBytes<const N: usize>(pub [u8; N]);

impl<const N: usize> core::fmt::Debug for FixedBytes<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl<const N: usize> Serialize for FixedBytes<N> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de, const N: usize> Deserialize<'de> for FixedBytes<N> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct V<const N: usize>;
        impl<const N: usize> serde::de::Visitor<'_> for V<N> {
            type Value = FixedBytes<N>;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "a byte string of {N} bytes")
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                <[u8; N]>::try_from(v)
                    .map(FixedBytes)
                    .map_err(|_| E::invalid_length(v.len(), &self))
            }
        }
        deserializer.deserialize_bytes(V::<N>)
    }
}

/// BEP 44 `get`.
#[derive(Debug, Serialize, Deserialize)]
pub struct GetRequest {
    pub id: Id20,
    pub target: Id20,
    /// Only send the value of a mutable item when its sequence number is greater than this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub want: Option<Want>,
}

/// BEP 44 `put`. `k`, `sig` and `seq` together make it a mutable put.
///
/// The sizes of `v` and `salt` are not limited here (only by the datagram) but by the code that
/// answers the query, because BEP 44 has error codes for them (205, 207): rejecting them here
/// would drop the query without an answer.
#[derive(Debug, Serialize, Deserialize)]
pub struct PutRequest<BufT: ByteBufT> {
    pub id: Id20,
    pub token: BufT,
    pub v: RawBencode<BufT>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub k: Option<FixedBytes<32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sig: Option<FixedBytes<64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub salt: Option<BufT>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cas: Option<i64>,
}

#[derive(Debug)]
pub struct Message<BufT: ByteBufT> {
    pub kind: MessageKind<BufT>,
    pub transaction_id: BufT,
    pub version: Option<BufT>,
    pub ip: Option<SocketAddr>,
}

impl Message<ByteBufOwned> {
    // This implies that the transaction id was generated by us.
    pub fn get_our_transaction_id(&self) -> Option<u16> {
        let tid = self.transaction_id.as_ref();
        if tid.len() != 2 {
            return None;
        }
        let tid = ((tid[0] as u16) << 8) + (tid[1] as u16);
        Some(tid)
    }
}

pub enum MessageKind<BufT: ByteBufT> {
    Error(ErrorDescription<BufT>),
    GetPeersRequest(GetPeersRequest),
    FindNodeRequest(FindNodeRequest),
    Response(Response<BufT>),
    PingRequest(PingRequest),
    AnnouncePeer(AnnouncePeer<BufT>),
    GetRequest(GetRequest),
    PutRequest(PutRequest<BufT>),
}

impl<BufT: ByteBufT> core::fmt::Debug for MessageKind<BufT> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Error(e) => write!(f, "{e:?}"),
            Self::GetPeersRequest(r) => write!(f, "{r:?}"),
            Self::FindNodeRequest(r) => write!(f, "{r:?}"),
            Self::Response(r) => write!(f, "{r:?}"),
            Self::PingRequest(r) => write!(f, "{r:?}"),
            Self::AnnouncePeer(r) => write!(f, "{r:?}"),
            Self::GetRequest(r) => write!(f, "{r:?}"),
            Self::PutRequest(r) => write!(f, "{r:?}"),
        }
    }
}

pub fn serialize_message<'a, W: Write, BufT: ByteBufT + From<&'a [u8]>>(
    writer: &mut W,
    transaction_id: BufT,
    version: Option<BufT>,
    ip: Option<SocketAddr>,
    kind: MessageKind<BufT>,
) -> crate::Result<()> {
    let ip = ip.map(Compact);
    match kind {
        MessageKind::Error(e) => {
            let msg: RawMessage<BufT, (), ()> = RawMessage {
                message_type: MessageType::Error,
                transaction_id,
                error: Some(e),
                response: None,
                method_name: None,
                version,
                ip,
                arguments: None,
            };
            Ok(bencode::bencode_serialize_to_writer(msg, writer)?)
        }
        MessageKind::GetPeersRequest(req) => {
            let msg: RawMessage<BufT, _, ()> = RawMessage {
                message_type: MessageType::Request,
                transaction_id,
                error: None,
                response: None,
                method_name: Some(BufT::from(b"get_peers")),
                arguments: Some(req),
                ip,
                version,
            };
            Ok(bencode::bencode_serialize_to_writer(msg, writer)?)
        }
        MessageKind::FindNodeRequest(req) => {
            let msg: RawMessage<BufT, _, ()> = RawMessage {
                message_type: MessageType::Request,
                transaction_id,
                error: None,
                response: None,
                method_name: Some(BufT::from(b"find_node")),
                arguments: Some(req),
                ip,
                version,
            };
            Ok(bencode::bencode_serialize_to_writer(msg, writer)?)
        }
        MessageKind::Response(resp) => {
            let msg: RawMessage<BufT, (), _> = RawMessage {
                message_type: MessageType::Response,
                transaction_id,
                error: None,
                response: Some(resp),
                method_name: None,
                arguments: None,
                ip,
                version,
            };
            Ok(bencode::bencode_serialize_to_writer(msg, writer)?)
        }
        MessageKind::PingRequest(ping) => {
            let msg: RawMessage<BufT, _, ()> = RawMessage {
                message_type: MessageType::Request,
                transaction_id,
                error: None,
                response: None,
                method_name: Some(BufT::from(b"ping")),
                arguments: Some(ping),
                ip,
                version,
            };
            Ok(bencode::bencode_serialize_to_writer(msg, writer)?)
        }
        MessageKind::AnnouncePeer(announce) => {
            let msg: RawMessage<BufT, _, ()> = RawMessage {
                message_type: MessageType::Request,
                transaction_id,
                error: None,
                response: None,
                method_name: Some(BufT::from(b"announce_peer")),
                arguments: Some(announce),
                ip,
                version,
            };
            Ok(bencode::bencode_serialize_to_writer(msg, writer)?)
        }
        MessageKind::GetRequest(req) => {
            let msg: RawMessage<BufT, _, ()> = RawMessage {
                message_type: MessageType::Request,
                transaction_id,
                error: None,
                response: None,
                method_name: Some(BufT::from(b"get")),
                arguments: Some(req),
                ip,
                version,
            };
            Ok(bencode::bencode_serialize_to_writer(msg, writer)?)
        }
        MessageKind::PutRequest(req) => {
            let msg: RawMessage<BufT, _, ()> = RawMessage {
                message_type: MessageType::Request,
                transaction_id,
                error: None,
                response: None,
                method_name: Some(BufT::from(b"put")),
                arguments: Some(req),
                ip,
                version,
            };
            Ok(bencode::bencode_serialize_to_writer(msg, writer)?)
        }
    }
}

pub fn deserialize_message<'de, BufT>(buf: &'de [u8]) -> anyhow::Result<Message<BufT>>
where
    BufT: ByteBufT + Deserialize<'de>,
{
    let de: RawMessage<ByteBuf> = bencode::from_bytes(buf).map_err(|e| e.into_anyhow())?;
    match de.message_type {
        MessageType::Request => match (&de.arguments, &de.method_name, &de.response, &de.error) {
            (Some(_), Some(method_name), None, None) => match method_name.as_ref() {
                b"find_node" => {
                    let de: RawMessage<BufT, FindNodeRequest> =
                        bencode::from_bytes(buf).map_err(|e| e.into_anyhow())?;
                    Ok(Message {
                        transaction_id: de.transaction_id,
                        version: de.version,
                        ip: de.ip.map(|c| c.0),
                        kind: MessageKind::FindNodeRequest(de.arguments.unwrap()),
                    })
                }
                b"get_peers" => {
                    let de: RawMessage<BufT, GetPeersRequest> =
                        bencode::from_bytes(buf).map_err(|e| e.into_anyhow())?;
                    Ok(Message {
                        transaction_id: de.transaction_id,
                        version: de.version,
                        ip: de.ip.map(|c| c.0),
                        kind: MessageKind::GetPeersRequest(de.arguments.unwrap()),
                    })
                }
                b"ping" => {
                    let de: RawMessage<BufT, PingRequest> =
                        bencode::from_bytes(buf).map_err(|e| e.into_anyhow())?;
                    Ok(Message {
                        transaction_id: de.transaction_id,
                        version: de.version,
                        ip: de.ip.map(|c| c.0),
                        kind: MessageKind::PingRequest(de.arguments.unwrap()),
                    })
                }
                b"announce_peer" => {
                    let de: RawMessage<BufT, AnnouncePeer<BufT>> =
                        bencode::from_bytes(buf).map_err(|e| e.into_anyhow())?;
                    Ok(Message {
                        transaction_id: de.transaction_id,
                        version: de.version,
                        ip: de.ip.map(|c| c.0),
                        kind: MessageKind::AnnouncePeer(de.arguments.unwrap()),
                    })
                }
                b"get" => {
                    let de: RawMessage<BufT, GetRequest> =
                        bencode::from_bytes(buf).map_err(|e| e.into_anyhow())?;
                    let req = de
                        .arguments
                        .ok_or_else(|| anyhow::anyhow!("get without arguments"))?;
                    Ok(Message {
                        transaction_id: de.transaction_id,
                        version: de.version,
                        ip: de.ip.map(|c| c.0),
                        kind: MessageKind::GetRequest(req),
                    })
                }
                b"put" => {
                    let de: RawMessage<BufT, PutRequest<BufT>> =
                        bencode::from_bytes(buf).map_err(|e| e.into_anyhow())?;
                    let req = de
                        .arguments
                        .ok_or_else(|| anyhow::anyhow!("put without arguments"))?;
                    Ok(Message {
                        transaction_id: de.transaction_id,
                        version: de.version,
                        ip: de.ip.map(|c| c.0),
                        kind: MessageKind::PutRequest(req),
                    })
                }
                other => anyhow::bail!("unsupported method {:?}", ByteBuf(other)),
            },
            _ => anyhow::bail!(
                "cannot deserialize message as request, expected exactly \"a\" and \"q\" to be set. Message: {:?}",
                de
            ),
        },
        MessageType::Response => match (&de.arguments, &de.method_name, &de.response, &de.error) {
            // some peers are sending method name against the protocol, so ignore it.
            (None, _, Some(_), None) => {
                let de: RawMessage<BufT, IgnoredAny, Response<BufT>> =
                    bencode::from_bytes(buf).map_err(|e| e.into_anyhow())?;
                let response = de
                    .response
                    .ok_or_else(|| anyhow::anyhow!("response without \"r\""))?;
                if let Some(v) = &response.v
                    && v.0.as_ref().len() > crate::bep44::MAX_VALUE_SIZE
                {
                    anyhow::bail!(
                        "\"v\" of {} bytes is over the BEP 44 limit of {}",
                        v.0.as_ref().len(),
                        crate::bep44::MAX_VALUE_SIZE
                    );
                }
                Ok(Message {
                    transaction_id: de.transaction_id,
                    version: de.version,
                    ip: de.ip.map(|c| c.0),
                    kind: MessageKind::Response(response),
                })
            }
            _ => anyhow::bail!(
                "cannot deserialize message as response, expected exactly \"r\" to be set. Message: {:?}",
                de
            ),
        },
        MessageType::Error => match (&de.arguments, &de.method_name, &de.response, &de.error) {
            // some peers are sending method name against the protocol, so ignore it.
            (None, _, None, Some(_)) => {
                let de: RawMessage<BufT, IgnoredAny, Response<BufT>> =
                    bencode::from_bytes(buf).map_err(|e| e.into_anyhow())?;
                Ok(Message {
                    transaction_id: de.transaction_id,
                    version: de.version,
                    ip: de.ip.map(|c| c.0),
                    kind: MessageKind::Error(de.error.unwrap()),
                })
            }
            _ => anyhow::bail!(
                "cannot deserialize message as error, expected exactly \"e\" to be set. Message: {:?}",
                de
            ),
        },
    }
}

/// The transaction id of a BEP 44 `put` query that [`deserialize_message`] refused, e.g. for a
/// `k` or `sig` of the wrong length. BEP 44 has such a put answered with error 203 rather than
/// dropped. `None` when `buf` is not a `put` query at all.
pub fn malformed_put_transaction_id(buf: &[u8]) -> Option<ByteBufOwned> {
    let de: RawMessage<ByteBuf> = bencode::from_bytes(buf).ok()?;
    match (de.message_type, de.method_name) {
        (MessageType::Request, Some(method)) if method.as_ref() == b"put" => {
            Some(ByteBufOwned::from(de.transaction_id.as_ref()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use crate::bprotocol::{self, Want};
    use bencode::{ByteBuf, bencode_serialize_to_writer};

    // Dumped with wireshark.
    const FIND_NODE_REQUEST: &[u8] =
        include_bytes!("../resources/test_requests/find_node_request.bin");
    const GET_PEERS_REQUEST_0: &[u8] =
        include_bytes!("../resources/test_requests/get_peers_request_0.bin");
    const GET_PEERS_REQUEST_1: &[u8] =
        include_bytes!("../resources/test_requests/get_peers_request_1.bin");
    const FIND_NODE_RESPONSE_1: &[u8] =
        include_bytes!("../resources/test_requests/find_node_response_1.bin");
    const FIND_NODE_RESPONSE_2: &[u8] =
        include_bytes!("../resources/test_requests/find_node_response_2.bin");
    const FIND_NODE_RESPONSE_3: &[u8] =
        include_bytes!("../resources/test_requests/find_node_response_3.bin");

    fn write(filename: &str, data: &[u8]) {
        let full = format!("/tmp/{filename}.bin");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(full)
            .unwrap();
        f.write_all(data).unwrap()
    }

    fn debug_bencode(name: &str, data: &[u8]) {
        println!(
            "{name}: {:#?}",
            bencode::dyn_from_bytes::<ByteBuf>(data).unwrap()
        );
    }

    fn test_deserialize_then_serialize(data: &[u8], name: &'static str) {
        dbg!(bencode::dyn_from_bytes::<ByteBuf>(data).unwrap());
        let bprotocol::Message {
            kind,
            transaction_id,
            version,
            ip,
        } = dbg!(bprotocol::deserialize_message::<ByteBuf>(data).unwrap());
        let mut buf = Vec::new();
        bprotocol::serialize_message(&mut buf, transaction_id, version, ip, kind).unwrap();

        if buf.as_slice() != data {
            write(&format!("{name}-serialized"), buf.as_slice());
            write(&format!("{name}-expected"), data);
            panic!(
                "{} results don't match, dumped to /tmp/{}-*.bin",
                name, name
            )
        }
    }

    #[test]
    fn serialize_then_deserialize_then_serialize_error() {
        let mut buf = Vec::new();
        let transaction_id = ByteBuf(b"123");
        bprotocol::serialize_message(
            &mut buf,
            transaction_id,
            None,
            None,
            bprotocol::MessageKind::Error(bprotocol::ErrorDescription {
                code: 201,
                description: ByteBuf(b"Some error"),
            }),
        )
        .unwrap();

        let bprotocol::Message {
            transaction_id,
            kind,
            ..
        } = bprotocol::deserialize_message::<ByteBuf>(&buf).unwrap();

        let mut buf2 = Vec::new();
        bprotocol::serialize_message(&mut buf2, transaction_id, None, None, kind).unwrap();

        if buf.as_slice() != buf2.as_slice() {
            write("error-serialized", buf.as_slice());
            write("error-serialized-again", buf2.as_slice());
            panic!("results don't match, dumped to /tmp/error-serialized-*.bin",)
        }
    }

    #[test]
    fn deserialize_request_find_node() {
        test_deserialize_then_serialize(FIND_NODE_REQUEST, "find_node_request")
    }

    #[test]
    fn deserialize_request_get_peers() {
        test_deserialize_then_serialize(GET_PEERS_REQUEST_0, "get_peers_request_0")
    }

    #[test]
    fn deserialize_response_find_node() {
        test_deserialize_then_serialize(FIND_NODE_RESPONSE_1, "find_node_response")
    }

    #[test]
    fn deserialize_response_find_node_2() {
        test_deserialize_then_serialize(FIND_NODE_RESPONSE_2, "find_node_response_2")
    }

    #[test]
    fn deserialize_response_find_node_3() {
        test_deserialize_then_serialize(FIND_NODE_RESPONSE_3, "find_node_response_3")
    }

    #[test]
    fn deserialize_request_get_peers_request_1() {
        test_deserialize_then_serialize(GET_PEERS_REQUEST_1, "get_peers_request_1")
    }

    #[test]
    fn test_announce() {
        let ann = b"d1:ad2:id20:abcdefghij012345678912:implied_porti1e9:info_hash20:mnopqrstuvwxyz1234564:porti6881e5:token8:aoeusnthe1:q13:announce_peer1:t2:aa1:y1:qe";
        let msg = bprotocol::deserialize_message::<ByteBuf>(ann).unwrap();
        match &msg.kind {
            bprotocol::MessageKind::AnnouncePeer(ann) => {
                dbg!(&ann);
            }
            _ => panic!("wrong kind"),
        }
        let mut buf = Vec::new();
        bprotocol::serialize_message(&mut buf, msg.transaction_id, msg.version, msg.ip, msg.kind)
            .unwrap();
        assert_eq!(ann[..], buf[..]);
    }

    #[test]
    fn deserialize_bencode_packets_captured_from_wireshark() {
        debug_bencode("req: find_node", FIND_NODE_REQUEST);
        debug_bencode("req: get_peers", GET_PEERS_REQUEST_0);
        debug_bencode("resp from the requesting node", FIND_NODE_RESPONSE_1);
        debug_bencode("resp from some random IP", FIND_NODE_RESPONSE_2);
        debug_bencode("another resp from some random IP", FIND_NODE_RESPONSE_3);
        debug_bencode("req to another node", GET_PEERS_REQUEST_1);
    }

    #[test]
    fn serde_want_deserialize() {
        assert_eq!(bencode::from_bytes::<Want>(b"l2:n4e").unwrap(), Want::V4);
        assert_eq!(bencode::from_bytes::<Want>(b"l2:n6e").unwrap(), Want::V6);
        assert_eq!(
            bencode::from_bytes::<Want>(b"l2:n42:n6e").unwrap(),
            Want::Both
        );
        assert_eq!(
            bencode::from_bytes::<Want>(b"l2:aa2:bbe").unwrap(),
            Want::None
        );
    }

    #[test]
    fn serde_want_serialize() {
        let mut w = Vec::new();
        bencode_serialize_to_writer(Want::V6, &mut w).unwrap();
        assert_eq!(&w, b"l2:n6e");

        let mut w = Vec::new();
        bencode_serialize_to_writer(Want::V4, &mut w).unwrap();
        assert_eq!(&w, b"l2:n4e");

        let mut w = Vec::new();
        bencode_serialize_to_writer(Want::Both, &mut w).unwrap();
        assert_eq!(&w, b"l2:n42:n6e");

        let mut w = Vec::new();
        bencode_serialize_to_writer(Want::None, &mut w).unwrap();
        assert_eq!(&w, b"le")
    }

    /// `b` as a bencoded byte string.
    fn bstr(b: &[u8]) -> Vec<u8> {
        let mut out = format!("{}:", b.len()).into_bytes();
        out.extend_from_slice(b);
        out
    }

    /// Concatenates the parts of a hand-written message.
    fn msg(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    const ID: &[u8; 20] = b"abcdefghij0123456789";
    const TARGET: &[u8; 20] = b"mnopqrstuvwxyz123456";
    const KEY: &[u8; 32] = &[0x77; 32];
    const SIG: &[u8; 64] = &[0x55; 64];
    // A dictionary whose keys are not sorted: not canonical bencode, so a value that went through
    // a parse and a re-serialization would come out different.
    const UNSORTED_V: &[u8] = b"d1:bi1e1:ai2ee";

    fn put_mutable_request(k: &[u8], salt: &[u8], sig: &[u8], v: &[u8]) -> Vec<u8> {
        msg(&[
            b"d1:ad3:casi3e2:id",
            &bstr(ID),
            b"1:k",
            &bstr(k),
            b"4:salt",
            &bstr(salt),
            b"3:seqi4e3:sig",
            &bstr(sig),
            b"5:token4:abcd1:v",
            v,
            b"e1:q3:put1:t2:aa1:y1:qe",
        ])
    }

    fn get_response(k: &[u8], sig: &[u8], v: &[u8]) -> Vec<u8> {
        msg(&[
            b"d1:rd2:id",
            &bstr(ID),
            b"1:k",
            &bstr(k),
            b"3:seqi4e3:sig",
            &bstr(sig),
            b"5:token4:abcd1:v",
            v,
            b"e1:t2:aa1:y1:re",
        ])
    }

    #[test]
    fn bep44_get_request_round_trip() {
        let data = msg(&[
            b"d1:ad2:id",
            &bstr(ID),
            b"3:seqi4e6:target",
            &bstr(TARGET),
            b"e1:q3:get1:t2:aa1:y1:qe",
        ]);
        match bprotocol::deserialize_message::<ByteBuf>(&data)
            .unwrap()
            .kind
        {
            bprotocol::MessageKind::GetRequest(r) => {
                assert_eq!(r.target.0, *TARGET);
                assert_eq!(r.seq, Some(4));
            }
            other => panic!("not a get: {other:?}"),
        }
        test_deserialize_then_serialize(&data, "bep44_get_request");
    }

    #[test]
    fn bep44_put_requests_round_trip() {
        let immutable = msg(&[
            b"d1:ad2:id",
            &bstr(ID),
            b"5:token4:abcd1:v12:Hello World!e1:q3:put1:t2:aa1:y1:qe",
        ]);
        match bprotocol::deserialize_message::<ByteBuf>(&immutable)
            .unwrap()
            .kind
        {
            bprotocol::MessageKind::PutRequest(r) => {
                assert_eq!(r.v.0.as_ref(), b"12:Hello World!");
                assert!(r.k.is_none() && r.sig.is_none() && r.seq.is_none());
            }
            other => panic!("not a put: {other:?}"),
        }
        test_deserialize_then_serialize(&immutable, "bep44_put_immutable");

        let mutable = put_mutable_request(KEY, b"foobar", SIG, UNSORTED_V);
        match bprotocol::deserialize_message::<ByteBuf>(&mutable)
            .unwrap()
            .kind
        {
            bprotocol::MessageKind::PutRequest(r) => {
                assert_eq!(r.v.0.as_ref(), UNSORTED_V);
                assert_eq!(r.k.unwrap().0, *KEY);
                assert_eq!(r.sig.unwrap().0, *SIG);
                assert_eq!((r.seq, r.cas), (Some(4), Some(3)));
                assert_eq!(r.salt.unwrap().as_ref(), b"foobar");
            }
            other => panic!("not a put: {other:?}"),
        }
        test_deserialize_then_serialize(&mutable, "bep44_put_mutable");
    }

    #[test]
    fn bep44_get_response_round_trip() {
        let data = get_response(KEY, SIG, UNSORTED_V);
        match bprotocol::deserialize_message::<ByteBuf>(&data)
            .unwrap()
            .kind
        {
            bprotocol::MessageKind::Response(r) => {
                assert_eq!(r.v.unwrap().0.as_ref(), UNSORTED_V);
                assert_eq!(r.k.unwrap().0, *KEY);
                assert_eq!(r.sig.unwrap().0, *SIG);
                assert_eq!(r.seq, Some(4));
            }
            other => panic!("not a response: {other:?}"),
        }
        test_deserialize_then_serialize(&data, "bep44_get_response");
    }

    #[test]
    fn bep44_rejects_mis_sized_fields() {
        let decodes = |data: &[u8]| bprotocol::deserialize_message::<ByteBuf>(data).is_ok();
        let v = b"i1e";

        // "k" is exactly 32 bytes, "sig" exactly 64, in queries and in responses.
        assert!(decodes(&put_mutable_request(KEY, b"", SIG, v)));
        assert!(!decodes(&put_mutable_request(&KEY[..31], b"", SIG, v)));
        assert!(!decodes(&put_mutable_request(&[0x77; 33], b"", SIG, v)));
        assert!(!decodes(&put_mutable_request(KEY, b"", &SIG[..63], v)));
        assert!(decodes(&get_response(KEY, SIG, v)));
        assert!(!decodes(&get_response(&KEY[..31], SIG, v)));
        assert!(!decodes(&get_response(KEY, &[0x55; 65], v)));

        // "v" must be one well-formed bencoded value.
        assert!(!decodes(&put_mutable_request(KEY, b"", SIG, b"i1x")));
        assert!(!decodes(&get_response(KEY, SIG, b"x")));

        // A response carries at most 1000 bytes of "v".
        let v_of_len = |len: usize| bstr(&vec![b'x'; len - 4]);
        assert_eq!(v_of_len(1000).len(), 1000);
        assert!(decodes(&get_response(KEY, SIG, &v_of_len(1000))));
        assert!(!decodes(&get_response(KEY, SIG, &v_of_len(1001))));

        // A put with too big a "v" or "salt" still decodes, so that it is answered with error
        // 205 / 207 (see bep44::check_put) instead of being dropped.
        assert!(decodes(&put_mutable_request(
            KEY,
            &[b's'; 65],
            SIG,
            &v_of_len(1001)
        )));
    }
}
