use buffers::{ByteBuf, ByteBufOwned};
use bytes::Bytes;
use clone_to_owned::CloneToOwned;
use librqbit_core::hash_id::Id32;

pub const MSG_HASH_REQUEST: u8 = 21;
pub const MSG_HASH_HASHES: u8 = 22;
pub const MSG_HASH_REJECT: u8 = 23;

/// Fixed-size payload for hash request/reject: 32 + 4*4 = 48 bytes
pub const HASH_REQUEST_PAYLOAD_LEN: usize = 48;
/// Minimum payload for hash hashes: 48 bytes header (no hashes)
pub const HASH_HASHES_MIN_PAYLOAD_LEN: usize = 48;

/// BEP 52 Hash Request (message ID 21).
///
/// Wire format: `[pieces_root:32][base_layer:4][index:4][length:4][proof_layers:4]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashRequest {
    pub pieces_root: Id32,
    pub base_layer: u32,
    pub index: u32,
    pub length: u32,
    pub proof_layers: u32,
}

impl HashRequest {
    pub fn serialize_to(&self, buf: &mut [u8]) {
        buf[0..32].copy_from_slice(&self.pieces_root.0);
        buf[32..36].copy_from_slice(&self.base_layer.to_be_bytes());
        buf[36..40].copy_from_slice(&self.index.to_be_bytes());
        buf[40..44].copy_from_slice(&self.length.to_be_bytes());
        buf[44..48].copy_from_slice(&self.proof_layers.to_be_bytes());
    }

    pub fn deserialize(payload: &[u8; HASH_REQUEST_PAYLOAD_LEN]) -> Self {
        HashRequest {
            pieces_root: Id32::new(payload[0..32].try_into().unwrap()),
            base_layer: u32::from_be_bytes(payload[32..36].try_into().unwrap()),
            index: u32::from_be_bytes(payload[36..40].try_into().unwrap()),
            length: u32::from_be_bytes(payload[40..44].try_into().unwrap()),
            proof_layers: u32::from_be_bytes(payload[44..48].try_into().unwrap()),
        }
    }
}

/// BEP 52 Hash Reject (message ID 23).
///
/// Sent when a hash request cannot be fulfilled. Same wire format as HashRequest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashReject {
    pub pieces_root: Id32,
    pub base_layer: u32,
    pub index: u32,
    pub length: u32,
    pub proof_layers: u32,
}

impl HashReject {
    pub fn serialize_to(&self, buf: &mut [u8]) {
        buf[0..32].copy_from_slice(&self.pieces_root.0);
        buf[32..36].copy_from_slice(&self.base_layer.to_be_bytes());
        buf[36..40].copy_from_slice(&self.index.to_be_bytes());
        buf[40..44].copy_from_slice(&self.length.to_be_bytes());
        buf[44..48].copy_from_slice(&self.proof_layers.to_be_bytes());
    }

    pub fn deserialize(payload: &[u8; HASH_REQUEST_PAYLOAD_LEN]) -> Self {
        HashReject {
            pieces_root: Id32::new(payload[0..32].try_into().unwrap()),
            base_layer: u32::from_be_bytes(payload[32..36].try_into().unwrap()),
            index: u32::from_be_bytes(payload[36..40].try_into().unwrap()),
            length: u32::from_be_bytes(payload[40..44].try_into().unwrap()),
            proof_layers: u32::from_be_bytes(payload[44..48].try_into().unwrap()),
        }
    }
}

impl From<HashRequest> for HashReject {
    fn from(req: HashRequest) -> Self {
        HashReject {
            pieces_root: req.pieces_root,
            base_layer: req.base_layer,
            index: req.index,
            length: req.length,
            proof_layers: req.proof_layers,
        }
    }
}

/// BEP 52 Hash Hashes (message ID 22).
///
/// Response containing the requested hashes plus proof hashes.
///
/// Wire format: `[pieces_root:32][base_layer:4][index:4][length:4][proof_layers:4][hash0:32]...[hashN:32]`
#[derive(Debug)]
pub struct HashHashes<B> {
    pub pieces_root: Id32,
    pub base_layer: u32,
    pub index: u32,
    pub length: u32,
    pub proof_layers: u32,
    pub hashes: B,
}

impl CloneToOwned for HashHashes<ByteBuf<'_>> {
    type Target = HashHashes<ByteBufOwned>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        HashHashes {
            pieces_root: self.pieces_root,
            base_layer: self.base_layer,
            index: self.index,
            length: self.length,
            proof_layers: self.proof_layers,
            hashes: self.hashes.clone_to_owned(within_buffer),
        }
    }
}

impl HashHashes<ByteBufOwned> {
    pub fn as_borrowed(&self) -> HashHashes<ByteBuf<'_>> {
        HashHashes {
            pieces_root: self.pieces_root,
            base_layer: self.base_layer,
            index: self.index,
            length: self.length,
            proof_layers: self.proof_layers,
            hashes: self.hashes.as_ref().into(),
        }
    }
}

impl<B: AsRef<[u8]>> HashHashes<B> {
    pub fn serialize_header_to(&self, buf: &mut [u8]) {
        buf[0..32].copy_from_slice(&self.pieces_root.0);
        buf[32..36].copy_from_slice(&self.base_layer.to_be_bytes());
        buf[36..40].copy_from_slice(&self.index.to_be_bytes());
        buf[40..44].copy_from_slice(&self.length.to_be_bytes());
        buf[44..48].copy_from_slice(&self.proof_layers.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pieces_root() -> Id32 {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        Id32::new(bytes)
    }

    #[test]
    fn test_hash_request_roundtrip() {
        let req = HashRequest {
            pieces_root: sample_pieces_root(),
            base_layer: 0,
            index: 5,
            length: 8,
            proof_layers: 2,
        };
        let mut buf = [0u8; HASH_REQUEST_PAYLOAD_LEN];
        req.serialize_to(&mut buf);
        let deserialized = HashRequest::deserialize(&buf);
        assert_eq!(req, deserialized);
    }

    #[test]
    fn test_hash_reject_roundtrip() {
        let reject = HashReject {
            pieces_root: sample_pieces_root(),
            base_layer: 1,
            index: 10,
            length: 4,
            proof_layers: 3,
        };
        let mut buf = [0u8; HASH_REQUEST_PAYLOAD_LEN];
        reject.serialize_to(&mut buf);
        let deserialized = HashReject::deserialize(&buf);
        assert_eq!(reject, deserialized);
    }

    #[test]
    fn test_hash_reject_from_request() {
        let req = HashRequest {
            pieces_root: sample_pieces_root(),
            base_layer: 0,
            index: 5,
            length: 8,
            proof_layers: 2,
        };
        let reject = HashReject::from(req.clone());
        assert_eq!(reject.pieces_root, req.pieces_root);
        assert_eq!(reject.base_layer, req.base_layer);
        assert_eq!(reject.index, req.index);
        assert_eq!(reject.length, req.length);
        assert_eq!(reject.proof_layers, req.proof_layers);
    }

    #[test]
    fn test_hash_hashes_roundtrip() {
        let root = sample_pieces_root();
        // 2 hashes of 32 bytes each
        let hash_data: Vec<u8> = (0..64).collect();

        let hashes = HashHashes {
            pieces_root: root,
            base_layer: 0,
            index: 0,
            length: 2,
            proof_layers: 0,
            hashes: ByteBuf(&hash_data),
        };

        let mut buf = [0u8; HASH_HASHES_MIN_PAYLOAD_LEN];
        hashes.serialize_header_to(&mut buf);

        // Verify header fields
        assert_eq!(Id32::new(buf[0..32].try_into().unwrap()), root);
        assert_eq!(u32::from_be_bytes(buf[32..36].try_into().unwrap()), 0);
        assert_eq!(u32::from_be_bytes(buf[36..40].try_into().unwrap()), 0);
        assert_eq!(u32::from_be_bytes(buf[40..44].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(buf[44..48].try_into().unwrap()), 0);
    }

    #[test]
    fn test_hash_hashes_zero_hashes() {
        let hashes = HashHashes {
            pieces_root: sample_pieces_root(),
            base_layer: 0,
            index: 0,
            length: 0,
            proof_layers: 0,
            hashes: ByteBuf(&[]),
        };

        let mut buf = [0u8; HASH_HASHES_MIN_PAYLOAD_LEN];
        hashes.serialize_header_to(&mut buf);

        assert_eq!(
            Id32::new(buf[0..32].try_into().unwrap()),
            sample_pieces_root()
        );
    }

    #[test]
    fn test_hash_hashes_clone_to_owned() {
        let hash_data: Vec<u8> = (0..64).collect();
        let hashes = HashHashes {
            pieces_root: sample_pieces_root(),
            base_layer: 0,
            index: 1,
            length: 2,
            proof_layers: 1,
            hashes: ByteBuf(&hash_data),
        };

        let owned = hashes.clone_to_owned(None);
        assert_eq!(owned.pieces_root, hashes.pieces_root);
        assert_eq!(owned.base_layer, hashes.base_layer);
        assert_eq!(owned.index, hashes.index);
        assert_eq!(owned.length, hashes.length);
        assert_eq!(owned.proof_layers, hashes.proof_layers);
        assert_eq!(owned.hashes.as_ref(), hash_data.as_slice());

        // Test as_borrowed round-trip
        let borrowed = owned.as_borrowed();
        assert_eq!(borrowed.pieces_root, hashes.pieces_root);
        assert_eq!(borrowed.hashes.0, hash_data.as_slice());
    }
}
