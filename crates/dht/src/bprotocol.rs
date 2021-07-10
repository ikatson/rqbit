use std::{
    marker::PhantomData,
    net::{Ipv4Addr, SocketAddrV4},
};

use bencode::ByteBuf;
use serde::{
    de::{IgnoredAny, Unexpected},
    Deserialize, Deserializer, Serialize,
};

#[derive(Debug)]
enum MessageType {
    Request,
    Response,
    Error,
}

pub struct Id20(pub [u8; 20]);

impl std::fmt::Debug for Id20 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<")?;
        for byte in self.0 {
            write!(f, "{:02x?}", byte)?;
        }
        write!(f, ">")?;
        Ok(())
    }
}

impl Serialize for Id20 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Id20 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Id20;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(formatter, "a 20 byte slice")
            }
            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v.len() != 20 {
                    return Err(E::invalid_length(20, &self));
                }
                let mut buf = [0u8; 20];
                buf.copy_from_slice(&v);
                Ok(Id20(buf))
            }
        }
        deserializer.deserialize_bytes(Visitor {})
    }
}

impl<'de> Deserialize<'de> for MessageType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
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
struct ErrorDescription<BufT> {
    code: i32,
    description: BufT,
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
}

#[derive(Debug)]
pub struct Node {
    pub id: Id20,
    pub addr: SocketAddrV4,
}

#[derive(Debug)]
pub struct CompactNodeInfo {
    pub nodes: Vec<Node>,
}

impl Serialize for CompactNodeInfo {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut buf = Vec::<u8>::with_capacity(self.nodes.len() * 26);
        for node in self.nodes.iter() {
            buf.extend_from_slice(&node.id.0);
            let ip_octets = node.addr.ip().octets();
            let port = node.addr.port();
            buf.extend_from_slice(&ip_octets);
            // BE encoding for port.
            buf.push((port >> 8) as u8);
            buf.push((port & 0xff) as u8);
        }
        serializer.serialize_bytes(&buf)
    }
}

impl<'de> Deserialize<'de> for CompactNodeInfo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = CompactNodeInfo;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(formatter, "compact node info with length multiple of 26")
            }
            fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v.len() % 26 != 0 {
                    return Err(E::invalid_length(v.len(), &self));
                }
                let mut buf = Vec::<Node>::with_capacity(v.len() / 26);
                for chunk in v.chunks_exact(26) {
                    let mut node_id = [0u8; 20];
                    node_id.copy_from_slice(&chunk[..20]);
                    let ip = Ipv4Addr::new(chunk[20], chunk[21], chunk[22], chunk[23]);
                    let port = ((chunk[24] as u16) << 8) + chunk[25] as u16;
                    buf.push(Node {
                        id: Id20(node_id),
                        addr: SocketAddrV4::new(ip, port),
                    })
                }
                Ok(CompactNodeInfo { nodes: buf })
            }
        }
        deserializer.deserialize_bytes(Visitor)
    }
}

#[derive(Debug)]
pub struct CompactPeerInfo {
    pub addr: SocketAddrV4,
}

impl Serialize for CompactPeerInfo {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let octets = self.addr.ip().octets();
        let port = self.addr.port();
        let buf = [
            octets[0],
            octets[1],
            octets[2],
            octets[3],
            (port >> 8) as u8,
            (port & 0xff) as u8,
        ];
        serializer.serialize_bytes(&buf)
    }
}

impl<'de> Deserialize<'de> for CompactPeerInfo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = CompactPeerInfo;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(formatter, "6 bytes of peer info")
            }
            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v.len() != 6 {
                    return Err(E::invalid_length(6, &self));
                }
                let ip = Ipv4Addr::new(v[0], v[1], v[2], v[3]);
                let port = ((v[4] as u16) << 8) + v[5] as u16;
                Ok(CompactPeerInfo {
                    addr: SocketAddrV4::new(ip, port),
                })
            }
        }
        deserializer.deserialize_bytes(Visitor {})
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FindNodeRequest {
    pub id: Id20,
    pub target: Id20,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response<BufT> {
    pub id: Id20,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nodes: Option<CompactNodeInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<CompactPeerInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<BufT>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GetPeersRequest {
    pub id: Id20,
    pub info_hash: Id20,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "BufT: AsRef<[u8]> + Serialize"))]
#[serde(bound(deserialize = "BufT: From<&'de [u8]> + Deserialize<'de>"))]
pub struct GetPeersResponse<BufT> {
    pub id: Id20,
    pub token: BufT,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<CompactPeerInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nodes: Option<CompactNodeInfo>,
}

#[derive(Debug)]
pub struct Message<BufT> {
    pub transaction_id: BufT,
    pub kind: MessageKind<BufT>,
}

#[derive(Debug)]
pub enum MessageKind<BufT> {
    Error(ErrorDescription<BufT>),
    GetPeersRequest(GetPeersRequest),
    FindNodeRequest(FindNodeRequest),
    Response(Response<BufT>),
}

pub fn deserialize_message<'de, BufT>(buf: &'de [u8]) -> anyhow::Result<Message<BufT>>
where
    BufT: Deserialize<'de> + AsRef<[u8]>,
{
    let de: RawMessage<BufT> = bencode::from_bytes(buf)?;
    match de.message_type {
        MessageType::Request => match (de.arguments, de.method_name, de.response, de.error) {
            (Some(_), Some(method_name), None, None) => match method_name.as_ref() {
                b"find_node" => {
                    let de: RawMessage<BufT, FindNodeRequest> = bencode::from_bytes(buf)?;
                    Ok(Message {
                        transaction_id: de.transaction_id,
                        kind: MessageKind::FindNodeRequest(de.arguments.unwrap()),
                    })
                }
                b"get_peers" => {
                    let de: RawMessage<BufT, GetPeersRequest> = bencode::from_bytes(buf)?;
                    Ok(Message {
                        transaction_id: de.transaction_id,
                        kind: MessageKind::GetPeersRequest(de.arguments.unwrap()),
                    })
                }
                other => anyhow::bail!("unsupported method {:?}", ByteBuf(other)),
            },
            _ => anyhow::bail!(
                "cannot deserialize message as request, expected exactly \"a\" and \"q\" to be set"
            ),
        },
        MessageType::Response => match (de.arguments, de.method_name, de.response, de.error) {
            (None, None, Some(_), None) => {
                let de: RawMessage<BufT, IgnoredAny, Response<BufT>> = bencode::from_bytes(buf)?;
                Ok(Message {
                    transaction_id: de.transaction_id,
                    kind: MessageKind::Response(de.response.unwrap()),
                })
            }
            _ => anyhow::bail!(
                "cannot deserialize message as response, expected exactly \"r\" to be set"
            ),
        },
        MessageType::Error => todo!(),
    }
}
