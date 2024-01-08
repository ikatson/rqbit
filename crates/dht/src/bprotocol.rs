use std::{
    io::Write,
    marker::PhantomData,
    net::{Ipv4Addr, SocketAddrV4},
};

use bencode::{ByteBuf, ByteString};
use clone_to_owned::CloneToOwned;
use librqbit_core::hash_id::Id20;
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
pub struct ErrorDescription<BufT> {
    pub code: i32,
    pub description: BufT,
}

impl<BufT> CloneToOwned for ErrorDescription<BufT>
where
    BufT: CloneToOwned,
{
    type Target = ErrorDescription<<BufT as CloneToOwned>::Target>;

    fn clone_to_owned(&self) -> Self::Target {
        ErrorDescription {
            code: self.code,
            description: self.description.clone_to_owned(),
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
    ip: Option<CompactPeerInfo>,
}

pub struct Node {
    pub id: Id20,
    pub addr: SocketAddrV4,
}

impl core::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}={:?}", self.addr, self.id)
    }
}

pub struct CompactNodeInfo {
    pub nodes: Vec<Node>,
}

impl core::fmt::Debug for CompactNodeInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.nodes)
    }
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
                        id: Id20::new(node_id),
                        addr: SocketAddrV4::new(ip, port),
                    })
                }
                Ok(CompactNodeInfo { nodes: buf })
            }
        }
        deserializer.deserialize_bytes(Visitor)
    }
}

pub struct CompactPeerInfo {
    pub addr: SocketAddrV4,
}

impl core::fmt::Debug for CompactPeerInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.addr)
    }
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

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Response<BufT> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<Vec<CompactPeerInfo>>,
    pub id: Id20,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nodes: Option<CompactNodeInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<BufT>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GetPeersRequest {
    pub id: Id20,
    pub info_hash: Id20,
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
    pub kind: MessageKind<BufT>,
    pub transaction_id: BufT,
    pub version: Option<BufT>,
    pub ip: Option<SocketAddrV4>,
}

impl Message<ByteString> {
    // This implies that the transaction id was generated by us.
    pub fn get_our_transaction_id(&self) -> Option<u16> {
        if self.transaction_id.len() != 2 {
            return None;
        }
        let tid = ((self.transaction_id[0] as u16) << 8) + (self.transaction_id[1] as u16);
        Some(tid)
    }
}

pub enum MessageKind<BufT> {
    Error(ErrorDescription<BufT>),
    GetPeersRequest(GetPeersRequest),
    FindNodeRequest(FindNodeRequest),
    Response(Response<BufT>),
    PingRequest(PingRequest),
    AnnouncePeer(AnnouncePeer<BufT>),
}

impl<BufT: core::fmt::Debug> core::fmt::Debug for MessageKind<BufT> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Error(e) => write!(f, "{e:?}"),
            Self::GetPeersRequest(r) => write!(f, "{r:?}"),
            Self::FindNodeRequest(r) => write!(f, "{r:?}"),
            Self::Response(r) => write!(f, "{r:?}"),
            Self::PingRequest(r) => write!(f, "{r:?}"),
            Self::AnnouncePeer(r) => write!(f, "{r:?}"),
        }
    }
}

pub fn serialize_message<'a, W: Write, BufT: Serialize + From<&'a [u8]>>(
    writer: &mut W,
    transaction_id: BufT,
    version: Option<BufT>,
    ip: Option<SocketAddrV4>,
    kind: MessageKind<BufT>,
) -> anyhow::Result<()> {
    let ip = ip.map(|ip| CompactPeerInfo { addr: ip });
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
    }
}

pub fn deserialize_message<'de, BufT>(buf: &'de [u8]) -> anyhow::Result<Message<BufT>>
where
    BufT: Deserialize<'de> + AsRef<[u8]>,
{
    let de: RawMessage<ByteBuf> = bencode::from_bytes(buf)?;
    match de.message_type {
        MessageType::Request => match (&de.arguments, &de.method_name, &de.response, &de.error) {
            (Some(_), Some(method_name), None, None) => match method_name.as_ref() {
                b"find_node" => {
                    let de: RawMessage<BufT, FindNodeRequest> = bencode::from_bytes(buf)?;
                    Ok(Message {
                        transaction_id: de.transaction_id,
                        version: de.version,
                        ip: de.ip.map(|c| c.addr),
                        kind: MessageKind::FindNodeRequest(de.arguments.unwrap()),
                    })
                }
                b"get_peers" => {
                    let de: RawMessage<BufT, GetPeersRequest> = bencode::from_bytes(buf)?;
                    Ok(Message {
                        transaction_id: de.transaction_id,
                        version: de.version,
                        ip: de.ip.map(|c| c.addr),
                        kind: MessageKind::GetPeersRequest(de.arguments.unwrap()),
                    })
                }
                b"ping" => {
                    let de: RawMessage<BufT, PingRequest> = bencode::from_bytes(buf)?;
                    Ok(Message {
                        transaction_id: de.transaction_id,
                        version: de.version,
                        ip: de.ip.map(|c| c.addr),
                        kind: MessageKind::PingRequest(de.arguments.unwrap()),
                    })
                }
                b"announce_peer" => {
                    let de: RawMessage<BufT, AnnouncePeer<BufT>> = bencode::from_bytes(buf)?;
                    Ok(Message {
                        transaction_id: de.transaction_id,
                        version: de.version,
                        ip: de.ip.map(|c| c.addr),
                        kind: MessageKind::AnnouncePeer(de.arguments.unwrap())
                    })
                }
                other => anyhow::bail!("unsupported method {:?}", ByteBuf(other)),
            },
            _ => anyhow::bail!(
                "cannot deserialize message as request, expected exactly \"a\" and \"q\" to be set. Message: {:?}", de
            ),
        },
        MessageType::Response => match (&de.arguments, &de.method_name, &de.response, &de.error) {
            // some peers are sending method name against the protocol, so ignore it.
            (None, _, Some(_), None) => {
                let de: RawMessage<BufT, IgnoredAny, Response<BufT>> = bencode::from_bytes(buf)?;
                Ok(Message {
                    transaction_id: de.transaction_id,
                    version: de.version,
                    ip: de.ip.map(|c| c.addr),
                    kind: MessageKind::Response(de.response.unwrap()),
                })
            }
            _ => anyhow::bail!(
                "cannot deserialize message as response, expected exactly \"r\" to be set. Message: {:?}", de
            ),
        },
        MessageType::Error => match (&de.arguments, &de.method_name, &de.response, &de.error) {
            // some peers are sending method name against the protocol, so ignore it.
            (None, _, None, Some(_)) => {
                let de: RawMessage<BufT, IgnoredAny, Response<BufT>> = bencode::from_bytes(buf)?;
                Ok(Message {
                    transaction_id: de.transaction_id,
                    version: de.version,
                    ip: de.ip.map(|c| c.addr),
                    kind: MessageKind::Error(de.error.unwrap()),
                })
            }
            _ => anyhow::bail!(
                "cannot deserialize message as error, expected exactly \"e\" to be set. Message: {:?}", de
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use crate::bprotocol;
    use bencode::ByteBuf;

    // Dumped with wireshark.
    const FIND_NODE_REQUEST: &[u8] = b"64313a6164323a696432303abd7b477cfbcd10f30b705da20201e7101d8df155363a74617267657432303abd7b477cfbcd10f30b705da20201e7101d8df15565313a71393a66696e645f6e6f6465313a74323a0005313a79313a7165";
    const GET_PEERS_REQUEST: &[u8] = b"64313a6164323a696432303abd7b477cfbcd10f30b705da20201e7101d8df155393a696e666f5f6861736832303acab507494d02ebb1178b38f2e9d7be299c86b86265313a71393a6765745f7065657273313a74323a0006313a79313a7165";
    const FIND_NODE_RESPONSE: &[u8] = b"64313a7264323a696432303a3c00727348b3b8ed70baa1e1411b3869d8481321353a6e6f6465733230383a67a312defb7d429086bfdcd5a209684ee13f59615cbe360bc8d567a312defb7d429086bfdcd5a209684ee13f59615cbe360bc8d567a312defb7d429086bfdcd5a209684ee13f59615cbe360bc8d567a312defb7d429086bfdcd5a209684ee13f59615cbe360bc8d567a312defb7d429086bfdcd5a209684ee13f59615cbe360bc8d567a312defb7d429086bfdcd5a209684ee13f59615cbe360bc8d567a312defb7d429086bfdcd5a209684ee13f59615cbe360bc8d567a312defb7d429086bfdcd5a209684ee13f59615cbe360bc8d565313a74323a0005313a76343a4a420000313a79313a7265";
    const FIND_NODE_RESPONSE_2: &[u8] = b"64323a6970363a081ab440e935313a7264323a696432303a32f54e697351ff4aec29cdbaabf2fbe3467cc267353a6e6f6465733431363a54133f7f6d77567ff210fe88d49839107d1a955956aaa625e9ee438e4a0af6b324d9672886052c856b26b25835a689afbbdf5436b643eb20605e1d18f848b32cd275a117afb52d3a474d18541ae18dd20d3fbd936983af4ea87135d785d0661de2f4c4bf7925c59269105c05caa68658851c018d8890f73604e334afdfb8e556fd7ca8f3e0211bd2af91c4af4eee69415a273c0bd1c2b02e8b9ba827139b6c6ebc6dcb6ee53aac3c5147530a432e1b62c9116e1316e9364d7fd2f10f2499f47e862d847937e39a51aed74bb6e8f1c491d520868f1893aaa007d1af19b5328f1b4840759e5743aa59a6bf090c76b846145c6895303b7a49be387fd609a9212eb6541b1ae1fd2ddcf776b4688dd359c8157120809ac8b6651e5e6e8d58b4a80fa124e1f4ed536d61e4ee25d5a702fc8ab70cdf45852708c999215cc406c4caa862bcd0a6b88e58128d2b280ac74631b3591ae1fa4484a5560c31de4fc046b97b4c6ac31dc324ab2ef20952049bfcecdbc8cf79e4cfd378a89779c605559b79b8ae25ba326249e5629f7b9cc0ad33143832e1bca63da63cdb8a940117f0adc2c41965313a74323a0002313a79313a7265";
    const FIND_NODE_RESPONSE_3: &[u8] = b"64323a6970363a081ab440e935313a7264323a696432303a32f54e697351ff4aec29cdbaabf2fbe3467cc267353a6e6f6465733431363a26d4302a32aecf28f3fee9f6caf8867d762e28b963b5a531c4917373b33fb43c9d7c0d3daf45ee22ab947d4511c054364d4a904464878fc4a31e88b41d7ea953f7dc91d8017dafee5d0f8a4d2fa19fd3ec1c37c6807cad0a5601698909e7a487532fb9408928afaa7ca5e376bee87c4caafa88f2f9a9cc2ed992cd48be68771b48bb6efc225561c00dc3f40d04ab08d93c21a1b89097bd06fa4d1d122d6f1d86e041a5525a69b26d265d039cd52c8bebc923bf1bc3e9f71c7ed05e349d54465cca22233147f21d4c1cc531e461254249ea653909abe367bc25efab70bbe28cd38cbafc2e6db11df5d66bc20bc8a4c9490d84bf29f09ceb44c230dd2ced8b5cec47c71ae1ff66e9ed230e165873b0bef32163ad52c66edce28a7c9c8ae8647af27ba1eac73737ac167e21ed9116b1ef8104a7c28f89606be6f36d7584b791128793e8f8a0e6b48897a6463532547e400ef3a7067237d4d77bf40f1c09773ea85dd269adf35eeebca89b6993cdb116c0512abc2cbc74973d5e5f09940d0bbdf4e047ce15101ae13d794b1230188404a9fd2a5a10ccefb0622057bc6d7eeae5fb8565313a74323a0003313a79313a7265";

    const WHAT_IS_THAT: &[u8]= b"64313a6164323a696432303abd7b477cfbcd10f30b705da20201e7101d8df155393a696e666f5f6861736832303acab507494d02ebb1178b38f2e9d7be299c86b86265313a71393a6765745f7065657273313a74323a0007313a79313a7165";

    fn write(filename: &str, data: &[u8]) {
        let full = format!("/tmp/{filename}.bin");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(full)
            .unwrap();
        f.write_all(data).unwrap()
    }

    fn debug_hex_bencode(name: &str, data: &[u8]) {
        println!("{name}");
        let data = hex::decode(data).unwrap();

        println!(
            "{:#?}",
            bencode::dyn_from_bytes::<ByteBuf>(data.as_slice()).unwrap()
        );
    }

    fn test_deserialize_then_serialize_hex(data: &[u8], name: &'static str) {
        test_deserialize_then_serialize(&hex::decode(data).unwrap(), name);
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
        test_deserialize_then_serialize_hex(FIND_NODE_REQUEST, "find_node_request")
    }

    #[test]
    fn deserialize_request_get_peers() {
        test_deserialize_then_serialize_hex(GET_PEERS_REQUEST, "get_peers_request")
    }

    #[test]
    fn deserialize_response_find_node() {
        test_deserialize_then_serialize_hex(FIND_NODE_RESPONSE, "find_node_response")
    }

    #[test]
    fn deserialize_response_find_node_2() {
        test_deserialize_then_serialize_hex(FIND_NODE_RESPONSE_2, "find_node_response_2")
    }

    #[test]
    fn deserialize_response_find_node_3() {
        test_deserialize_then_serialize_hex(FIND_NODE_RESPONSE_3, "find_node_response_3")
    }

    #[test]
    fn deserialize_request_what_is_that() {
        test_deserialize_then_serialize_hex(WHAT_IS_THAT, "what_is_that")
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
        debug_hex_bencode("req: find_node", FIND_NODE_REQUEST);
        debug_hex_bencode("req: get_peers", GET_PEERS_REQUEST);
        debug_hex_bencode("resp from the requesting node", FIND_NODE_RESPONSE);
        debug_hex_bencode("resp from some random IP", FIND_NODE_RESPONSE_2);
        debug_hex_bencode("another resp from some random IP", FIND_NODE_RESPONSE_3);
        debug_hex_bencode("req to another node", WHAT_IS_THAT);
    }
}
