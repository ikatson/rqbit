use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize};

/// BEP-0005 DHT bootstrap node entry: ["<host>", <port>].
/// Custom serde to serialize as a 2-element bencode list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhtNode {
    pub host: String,
    pub port: u16,
}

impl DhtNode {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }
}

impl Serialize for DhtNode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut seq = serializer.serialize_seq(Some(2))?;
        seq.serialize_element(&self.host)?;
        seq.serialize_element(&self.port)?;
        seq.end()
    }
}

impl<'de> Deserialize<'de> for DhtNode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DhtNodeVisitor;
        impl<'de> serde::de::Visitor<'de> for DhtNodeVisitor {
            type Value = DhtNode;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "a list of [host, port]")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<DhtNode, A::Error> {
                let host: String = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
                let port: u16 = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(1, &self))?;

                seq.next_element::<serde::de::IgnoredAny>()?;
                Ok(DhtNode { host, port })
            }
        }
        deserializer.deserialize_seq(DhtNodeVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serialize(value: &impl Serialize) -> Vec<u8> {
        let mut buf = Vec::new();
        bencode::bencode_serialize_to_writer(value, &mut buf).unwrap();
        buf
    }

    #[test]
    fn test_roundtrip_bencode_single() {
        let node = DhtNode::new("127.0.0.1", 6881);
        let bytes = serialize(&node);
        assert_eq!(&bytes, b"l9:127.0.0.1i6881ee");
        let decoded: DhtNode = bencode::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, node);
    }

    #[test]
    fn test_roundtrip_bencode_vec() {
        let nodes = vec![
            DhtNode::new("127.0.0.1", 6881),
            DhtNode::new("10.0.0.1", 50001),
        ];
        let bytes = serialize(&nodes);
        let decoded: Vec<DhtNode> = bencode::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, nodes);
    }

    #[test]
    fn test_roundtrip_bencode_in_struct() {
        #[derive(Debug, PartialEq, Eq, serde_derive::Serialize, serde_derive::Deserialize)]
        struct Outer {
            name: String,
            nodes: Vec<DhtNode>,
            extra: u32,
        }

        let outer = Outer {
            name: "test".into(),
            nodes: vec![DhtNode::new("192.168.1.1", 8080)],
            extra: 42,
        };
        let bytes = serialize(&outer);
        let decoded: Outer = bencode::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, outer);
    }

    #[test]
    fn test_deserialize_empty_vec() {
        let bytes = serialize(&Vec::<DhtNode>::new());
        assert_eq!(&bytes, b"le");
        let decoded: Vec<DhtNode> = bencode::from_bytes(&bytes).unwrap();
        assert!(decoded.is_empty());
    }
}
