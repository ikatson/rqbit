//! BEP 44 (storing arbitrary data in the DHT): the targets and signatures of immutable and
//! mutable items, the checks of an incoming `put`, and the store that keeps what other nodes put
//! to us.
//!
//! <https://www.bittorrent.org/beps/bep_0044.html>

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use bencode::ByteBufOwned;
use bytes::Bytes;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use librqbit_core::hash_id::Id20;
use parking_lot::Mutex;
use sha1w::{ISha1, Sha1};

use crate::bprotocol::PutRequest;

/// The largest bencoded value (`v`) an item may have.
pub const MAX_VALUE_SIZE: usize = 1000;
/// The longest salt a mutable item may have.
pub const MAX_SALT_SIZE: usize = 64;
/// An item is forgotten this long after the last put of it.
pub(crate) const ITEM_LIFETIME: Duration = Duration::from_secs(2 * 60 * 60);

// libtorrent keeps 700 items by default (`dht_max_dht_items`). The byte limit keeps a store of
// 700 maximum-size items from taking more than a few hundred KiB.
const DEFAULT_MAX_ITEMS: usize = 700;
const DEFAULT_MAX_BYTES: usize = 256 * 1024;

/// The target (DHT key) of an immutable item: the SHA-1 of its bencoded value.
pub fn immutable_target(value: &[u8]) -> Id20 {
    let mut h = Sha1::new();
    h.update(value);
    Id20::new(h.finish())
}

/// The target (DHT key) of a mutable item: the SHA-1 of the public key followed by the salt.
pub fn mutable_target(public_key: &[u8; 32], salt: &[u8]) -> Id20 {
    let mut h = Sha1::new();
    h.update(public_key);
    h.update(salt);
    Id20::new(h.finish())
}

/// The bytes the signature of a mutable item covers: the salt (only when there is one), the
/// sequence number and the bencoded value, each as a bencoded dictionary entry.
pub(crate) fn signature_buffer(salt: &[u8], seq: i64, value: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(salt.len() + value.len() + 48);
    if !salt.is_empty() {
        buf.extend_from_slice(b"4:salt");
        buf.extend_from_slice(salt.len().to_string().as_bytes());
        buf.push(b':');
        buf.extend_from_slice(salt);
    }
    buf.extend_from_slice(b"3:seqi");
    buf.extend_from_slice(seq.to_string().as_bytes());
    buf.extend_from_slice(b"e1:v");
    buf.extend_from_slice(value);
    buf
}

/// Whether `signature` is the signature of `public_key` over `salt`, `seq` and `value`.
pub(crate) fn verify_mutable(
    public_key: &[u8; 32],
    salt: &[u8],
    seq: i64,
    value: &[u8],
    signature: &[u8; 64],
) -> bool {
    let Ok(key) = VerifyingKey::from_bytes(public_key) else {
        return false;
    };
    key.verify(
        &signature_buffer(salt, seq, value),
        &Signature::from_bytes(signature),
    )
    .is_ok()
}

/// The public key of the ed25519 key whose 32-byte secret (seed) is `signing_key`: the key a
/// mutable item put with [`crate::DhtState::put_mutable`] is found under.
pub fn public_key(signing_key: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(signing_key)
        .verifying_key()
        .to_bytes()
}

/// Signs a mutable item with the ed25519 key whose 32-byte secret (seed) is `signing_key`.
pub(crate) fn sign_mutable(
    signing_key: &[u8; 32],
    salt: &[u8],
    seq: i64,
    value: &[u8],
) -> [u8; 64] {
    SigningKey::from_bytes(signing_key)
        .sign(&signature_buffer(salt, seq, value))
        .to_bytes()
}

/// Whether `value` is exactly one bencoded value, as `v` must be.
pub(crate) fn is_single_bencoded_value(value: &[u8]) -> bool {
    bencode::from_bytes::<serde::de::IgnoredAny>(value).is_ok()
}

/// A mutable item as found in the DHT, with everything needed to check (and re-publish) it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutableItem {
    /// The bencoded value, byte for byte as it was signed.
    pub value: Bytes,
    /// The sequence number; a newer version of the item has a greater one.
    pub seq: i64,
    /// The ed25519 signature over the salt, `seq` and `value`.
    pub signature: [u8; 64],
    /// The ed25519 public key of the owner of the item.
    pub public_key: [u8; 32],
}

/// Why we refuse a `put`, as a KRPC error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PutError {
    /// 202: we have no room for the item and every item we have is closer to us.
    StorageFull,
    /// 203: a malformed put.
    Protocol(&'static str),
    /// 205: `v` is over [`MAX_VALUE_SIZE`].
    ValueTooBig,
    /// 206: the signature of a mutable item does not verify.
    InvalidSignature,
    /// 207: `salt` is over [`MAX_SALT_SIZE`].
    SaltTooBig,
    /// 301: `cas` does not match the sequence number of the item we have.
    CasMismatch,
    /// 302: the item we have has a greater sequence number.
    SequenceTooOld,
}

impl PutError {
    pub(crate) fn code(&self) -> i32 {
        match self {
            PutError::StorageFull => 202,
            PutError::Protocol(_) => 203,
            PutError::ValueTooBig => 205,
            PutError::InvalidSignature => 206,
            PutError::SaltTooBig => 207,
            PutError::CasMismatch => 301,
            PutError::SequenceTooOld => 302,
        }
    }

    pub(crate) fn message(&self) -> &'static str {
        match self {
            PutError::StorageFull => "storage full",
            PutError::Protocol(msg) => msg,
            PutError::ValueTooBig => "message (v field) too big",
            PutError::InvalidSignature => "invalid signature",
            PutError::SaltTooBig => "salt (salt field) too big",
            PutError::CasMismatch => "the CAS hash mismatched, re-read value and try again",
            PutError::SequenceTooOld => "sequence number less than current",
        }
    }
}

/// The mutable part of a checked put.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MutablePut {
    pub public_key: [u8; 32],
    pub signature: [u8; 64],
    pub seq: i64,
    pub salt: Bytes,
    pub cas: Option<i64>,
}

/// A put whose sizes and signature have been checked, ready for [`ItemStore::put`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PutItem {
    pub target: Id20,
    pub value: Bytes,
    pub mutable: Option<MutablePut>,
}

/// The checks of a put that need nothing but the put itself: sizes, the fields a mutable put
/// needs, and its signature. The token is checked by the caller, before this (verifying a
/// signature costs far more than looking up a token).
pub(crate) fn check_put(req: &PutRequest<ByteBufOwned>) -> Result<PutItem, PutError> {
    let value = req.v.0.0.clone();
    if value.len() > MAX_VALUE_SIZE {
        return Err(PutError::ValueTooBig);
    }
    let (k, sig, seq) = match (&req.k, &req.sig, req.seq) {
        (None, None, _) => {
            return Ok(PutItem {
                target: immutable_target(&value),
                value,
                mutable: None,
            });
        }
        (Some(k), Some(sig), Some(seq)) => (k.0, sig.0, seq),
        _ => {
            return Err(PutError::Protocol(
                "a mutable put needs \"k\", \"sig\" and \"seq\"",
            ));
        }
    };
    let salt = req.salt.as_ref().map(|s| s.0.clone()).unwrap_or_default();
    if salt.len() > MAX_SALT_SIZE {
        return Err(PutError::SaltTooBig);
    }
    if seq < 0 {
        return Err(PutError::Protocol("invalid (negative) sequence number"));
    }
    if !verify_mutable(&k, &salt, seq, &value, &sig) {
        return Err(PutError::InvalidSignature);
    }
    Ok(PutItem {
        target: mutable_target(&k, &salt),
        value,
        mutable: Some(MutablePut {
            public_key: k,
            signature: sig,
            seq,
            salt,
            cas: req.cas,
        }),
    })
}

struct MutableMeta {
    public_key: [u8; 32],
    signature: [u8; 64],
    seq: i64,
    salt: Bytes,
}

struct StoredItem {
    value: Bytes,
    mutable: Option<MutableMeta>,
    last_put: Instant,
}

impl StoredItem {
    fn size(&self) -> usize {
        self.value.len() + self.mutable.as_ref().map(|m| m.salt.len()).unwrap_or(0)
    }

    fn is_expired(&self, now: Instant) -> bool {
        now.duration_since(self.last_put) >= ITEM_LIFETIME
    }
}

/// What we have stored under a target, as `get` answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoredView {
    Immutable(Bytes),
    Mutable(MutableItem),
}

#[derive(Default)]
struct StoreInner {
    items: HashMap<Id20, StoredItem>,
    bytes: usize,
}

impl StoreInner {
    fn remove(&mut self, target: &Id20) {
        if let Some(item) = self.items.remove(target) {
            self.bytes -= item.size();
        }
    }

    fn expire(&mut self, now: Instant) {
        let mut bytes = self.bytes;
        self.items.retain(|_, item| {
            let keep = !item.is_expired(now);
            if !keep {
                bytes -= item.size();
            }
            keep
        });
        self.bytes = bytes;
    }
}

/// How far `target` is from us: the distance to the closer of our node ids.
fn distance_from_us(target: &Id20, ids: &[Id20; 2]) -> Id20 {
    target.distance(&ids[0]).min(target.distance(&ids[1]))
}

/// The items other nodes put to us. Bounded in number and bytes; items expire
/// [`ITEM_LIFETIME`] after their last put. Nothing is persisted.
pub(crate) struct ItemStore {
    max_items: usize,
    max_bytes: usize,
    inner: Mutex<StoreInner>,
}

impl Default for ItemStore {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_ITEMS, DEFAULT_MAX_BYTES)
    }
}

impl ItemStore {
    pub(crate) fn new(max_items: usize, max_bytes: usize) -> Self {
        Self {
            max_items,
            max_bytes,
            inner: Mutex::new(StoreInner::default()),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.inner.lock().items.len()
    }

    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.inner.lock().bytes
    }

    pub(crate) fn get(&self, target: &Id20, now: Instant) -> Option<StoredView> {
        let inner = self.inner.lock();
        let item = inner.items.get(target)?;
        if item.is_expired(now) {
            return None;
        }
        Some(match &item.mutable {
            None => StoredView::Immutable(item.value.clone()),
            Some(m) => StoredView::Mutable(MutableItem {
                value: item.value.clone(),
                seq: m.seq,
                signature: m.signature,
                public_key: m.public_key,
            }),
        })
    }

    /// Stores (or refreshes) a checked item. `ids` are our node ids: when the store is full, the
    /// item farthest from them goes, unless that is the new item itself.
    pub(crate) fn put(&self, item: PutItem, ids: [Id20; 2], now: Instant) -> Result<(), PutError> {
        let mut inner = self.inner.lock();
        inner.expire(now);

        let new_size = item.value.len() + item.mutable.as_ref().map(|m| m.salt.len()).unwrap_or(0);
        let (added_items, old_size) = match inner.items.get_mut(&item.target) {
            Some(existing) => {
                match (&item.mutable, &existing.mutable) {
                    // Same target, same value: a refresh.
                    (None, None) => {
                        existing.last_put = now;
                        return Ok(());
                    }
                    (Some(new), Some(old)) => {
                        if let Some(cas) = new.cas
                            && cas != old.seq
                        {
                            return Err(PutError::CasMismatch);
                        }
                        if new.seq < old.seq {
                            return Err(PutError::SequenceTooOld);
                        }
                        // The same sequence number keeps what we have (like libtorrent): the
                        // put only refreshes it.
                        if new.seq == old.seq {
                            existing.last_put = now;
                            return Ok(());
                        }
                    }
                    // A SHA-1 of a value equal to the SHA-1 of a key and salt.
                    _ => {
                        return Err(PutError::Protocol(
                            "target is taken by an item of the other kind",
                        ));
                    }
                }
                (0, existing.size())
            }
            None => (1, 0),
        };

        // Make room, farthest items first.
        let our_distance = distance_from_us(&item.target, &ids);
        while inner.items.len() + added_items > self.max_items
            || inner.bytes - old_size + new_size > self.max_bytes
        {
            let farthest = inner
                .items
                .keys()
                .filter(|t| **t != item.target)
                .map(|t| (distance_from_us(t, &ids), *t))
                .max();
            match farthest {
                Some((distance, t)) if distance > our_distance => inner.remove(&t),
                _ => return Err(PutError::StorageFull),
            }
        }

        let stored = StoredItem {
            value: item.value,
            mutable: item.mutable.map(|m| MutableMeta {
                public_key: m.public_key,
                signature: m.signature,
                seq: m.seq,
                salt: m.salt,
            }),
            last_put: now,
        };
        inner.bytes = inner.bytes - old_size + new_size;
        inner.items.insert(item.target, stored);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use bencode::ByteBufOwned;

    use super::*;
    use crate::bprotocol::{FixedBytes, PutRequest, RawBencode};

    fn unhex<const N: usize>(s: &str) -> [u8; N] {
        let mut out = [0u8; N];
        assert_eq!(s.len(), N * 2);
        for (i, o) in out.iter_mut().enumerate() {
            *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
        }
        out
    }

    const VALUE: &[u8] = b"12:Hello World!";
    const PUBLIC_KEY: &str = "77ff84905a91936367c01360803104f92432fcd904a43511876df5cdf3e7e548";
    const SIG_NO_SALT: &str = "305ac8aeb6c9c151fa120f120ea2cfb923564e11552d06a5d856091e5e853cff\
                               1260d3f39e4999684aa92eb73ffd136e6f4f3ecbfda0ce53a1608ecd7ae21f01";
    const SIG_SALT: &str = "6834284b6b24c3204eb2fea824d82f88883a3d95e8b4a21b8c0ded553d17d17d\
                            df9a8a7104b1258f30bed3787e6cb896fca78c58f8e03b5f18f14951a87d9a08";

    #[test]
    fn bep44_vector_mutable() {
        let pk = unhex::<32>(PUBLIC_KEY);
        let sig = unhex::<64>(SIG_NO_SALT);
        assert_eq!(
            signature_buffer(b"", 1, VALUE),
            b"3:seqi1e1:v12:Hello World!".to_vec()
        );
        assert_eq!(
            mutable_target(&pk, b""),
            Id20::from_str("4a533d47ec9c7d95b1ad75f576cffc641853b750").unwrap()
        );
        assert!(verify_mutable(&pk, b"", 1, VALUE, &sig));
        // Any change to what is signed breaks the signature.
        assert!(!verify_mutable(&pk, b"", 2, VALUE, &sig));
        assert!(!verify_mutable(&pk, b"", 1, b"12:Hello World?", &sig));
        assert!(!verify_mutable(&pk, b"x", 1, VALUE, &sig));
    }

    #[test]
    fn bep44_vector_mutable_with_salt() {
        let pk = unhex::<32>(PUBLIC_KEY);
        let sig = unhex::<64>(SIG_SALT);
        assert_eq!(
            signature_buffer(b"foobar", 1, VALUE),
            b"4:salt6:foobar3:seqi1e1:v12:Hello World!".to_vec()
        );
        assert_eq!(
            mutable_target(&pk, b"foobar"),
            Id20::from_str("411eba73b6f087ca51a3795d9c8c938d365e32c1").unwrap()
        );
        assert!(verify_mutable(&pk, b"foobar", 1, VALUE, &sig));
        assert!(!verify_mutable(&pk, b"", 1, VALUE, &sig));
        let mut forged = sig;
        forged[0] ^= 1;
        assert!(!verify_mutable(&pk, b"foobar", 1, VALUE, &forged));
    }

    #[test]
    fn bep44_vector_immutable() {
        assert_eq!(
            immutable_target(VALUE),
            Id20::from_str("e5f96f6f38320f0f33959cb4d3d656452117aadb").unwrap()
        );
    }

    #[test]
    fn sign_then_verify() {
        let seed = [7u8; 32];
        let pk = public_key(&seed);
        let sig = sign_mutable(&seed, b"salt", 42, VALUE);
        assert!(verify_mutable(&pk, b"salt", 42, VALUE, &sig));
        assert!(!verify_mutable(&pk, b"salt", 43, VALUE, &sig));
        assert!(!verify_mutable(
            &public_key(&[8u8; 32]),
            b"salt",
            42,
            VALUE,
            &sig
        ));
    }

    #[test]
    fn single_bencoded_value() {
        assert!(is_single_bencoded_value(b"12:Hello World!"));
        assert!(is_single_bencoded_value(b"d1:ai1ee"));
        assert!(!is_single_bencoded_value(b"i1ei2e"));
        assert!(!is_single_bencoded_value(b"12:Hello"));
        assert!(!is_single_bencoded_value(b""));
    }

    /// The signing key seed, salt, `seq` and `cas` of a mutable put.
    type MutableArgs<'a> = (&'a [u8; 32], &'a [u8], i64, Option<i64>);

    fn put_request(v: &[u8], mutable: Option<MutableArgs<'_>>) -> PutRequest<ByteBufOwned> {
        let (k, sig, seq, salt, cas) = match mutable {
            None => (None, None, None, None, None),
            Some((seed, salt, seq, cas)) => (
                Some(FixedBytes(public_key(seed))),
                Some(FixedBytes(sign_mutable(seed, salt, seq, v))),
                Some(seq),
                (!salt.is_empty()).then(|| ByteBufOwned::from(salt)),
                cas,
            ),
        };
        PutRequest {
            id: Id20::new([1; 20]),
            token: ByteBufOwned::from(&b"tokn"[..]),
            v: RawBencode(ByteBufOwned::from(v)),
            k,
            sig,
            seq,
            salt,
            cas,
        }
    }

    /// A bencoded byte string that is `len` bytes long in total, header included.
    fn value_of_len(len: usize) -> Vec<u8> {
        let body = (1..len)
            .find(|n| n.to_string().len() + 1 + n == len)
            .unwrap();
        let mut v = format!("{body}:").into_bytes();
        v.resize(len, b'x');
        assert!(is_single_bencoded_value(&v));
        v
    }

    #[test]
    fn check_put_errors() {
        let seed = [3u8; 32];

        let ok = check_put(&put_request(VALUE, None)).unwrap();
        assert_eq!(ok.target, immutable_target(VALUE));
        assert!(ok.mutable.is_none());

        let ok = check_put(&put_request(VALUE, Some((&seed, b"s", 5, Some(4))))).unwrap();
        assert_eq!(ok.target, mutable_target(&public_key(&seed), b"s"));
        assert_eq!(ok.mutable.as_ref().unwrap().cas, Some(4));

        // The limits themselves are fine.
        let max = value_of_len(MAX_VALUE_SIZE);
        check_put(&put_request(&max, None)).unwrap();
        check_put(&put_request(
            &max,
            Some((&seed, &[b's'; MAX_SALT_SIZE], 1, None)),
        ))
        .unwrap();

        let too_big = value_of_len(MAX_VALUE_SIZE + 1);
        assert_eq!(
            check_put(&put_request(&too_big, None)).unwrap_err().code(),
            205
        );
        assert_eq!(
            check_put(&put_request(&too_big, Some((&seed, b"", 1, None))))
                .unwrap_err()
                .code(),
            205
        );
        assert_eq!(
            check_put(&put_request(
                VALUE,
                Some((&seed, &[b's'; MAX_SALT_SIZE + 1], 1, None))
            ))
            .unwrap_err(),
            PutError::SaltTooBig
        );
        assert_eq!(
            check_put(&put_request(VALUE, Some((&seed, b"", -1, None))))
                .unwrap_err()
                .code(),
            203
        );

        // A mutable put without one of its fields.
        let mut incomplete = put_request(VALUE, Some((&seed, b"", 1, None)));
        incomplete.sig = None;
        assert_eq!(check_put(&incomplete).unwrap_err().code(), 203);

        // A signature that does not match.
        let mut forged = put_request(VALUE, Some((&seed, b"", 1, None)));
        forged.seq = Some(2);
        assert_eq!(check_put(&forged).unwrap_err(), PutError::InvalidSignature);
        assert_eq!(PutError::InvalidSignature.code(), 206);
        let mut other_key = put_request(VALUE, Some((&seed, b"", 1, None)));
        other_key.k = Some(FixedBytes(public_key(&[4u8; 32])));
        assert_eq!(check_put(&other_key).unwrap_err().code(), 206);
    }

    fn mutable_put(seed: &[u8; 32], v: &[u8], seq: i64, cas: Option<i64>) -> PutItem {
        check_put(&put_request(v, Some((seed, b"salt", seq, cas)))).unwrap()
    }

    const IDS: [Id20; 2] = [
        librqbit_core::hash_id::Id([0; 20]),
        librqbit_core::hash_id::Id([0; 20]),
    ];

    #[test]
    fn store_get_immutable_and_mutable() {
        let store = ItemStore::default();
        let now = Instant::now();
        let imm = check_put(&put_request(VALUE, None)).unwrap();
        store.put(imm.clone(), IDS, now).unwrap();
        assert_eq!(
            store.get(&imm.target, now),
            Some(StoredView::Immutable(Bytes::from_static(VALUE)))
        );

        let seed = [9u8; 32];
        let m = mutable_put(&seed, b"1:a", 1, None);
        store.put(m.clone(), IDS, now).unwrap();
        match store.get(&m.target, now) {
            Some(StoredView::Mutable(item)) => {
                assert_eq!(item.value, Bytes::from_static(b"1:a"));
                assert_eq!(item.seq, 1);
                assert_eq!(item.public_key, public_key(&seed));
                assert!(verify_mutable(
                    &item.public_key,
                    b"salt",
                    item.seq,
                    &item.value,
                    &item.signature
                ));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(store.len(), 2);
        assert_eq!(store.bytes(), VALUE.len() + 3 + 4);
        assert_eq!(store.get(&Id20::new([0x55; 20]), now), None);
    }

    #[test]
    fn store_seq_and_cas() {
        let store = ItemStore::default();
        let now = Instant::now();
        let seed = [9u8; 32];
        let first = mutable_put(&seed, b"1:a", 5, None);
        let target = first.target;
        store.put(first, IDS, now).unwrap();

        let value_of = |store: &ItemStore| match store.get(&target, now) {
            Some(StoredView::Mutable(item)) => (item.seq, item.value),
            other => panic!("unexpected {other:?}"),
        };

        // Going backwards is refused.
        assert_eq!(
            store.put(mutable_put(&seed, b"1:b", 4, None), IDS, now),
            Err(PutError::SequenceTooOld)
        );
        assert_eq!(PutError::SequenceTooOld.code(), 302);
        // The same sequence number keeps what we have.
        store
            .put(mutable_put(&seed, b"1:c", 5, None), IDS, now)
            .unwrap();
        assert_eq!(value_of(&store), (5, Bytes::from_static(b"1:a")));
        // A cas that does not match the current seq is refused, even with a newer seq.
        assert_eq!(
            store.put(mutable_put(&seed, b"1:d", 7, Some(4)), IDS, now),
            Err(PutError::CasMismatch)
        );
        assert_eq!(PutError::CasMismatch.code(), 301);
        assert_eq!(value_of(&store), (5, Bytes::from_static(b"1:a")));
        // A matching cas with a newer seq replaces the item.
        store
            .put(mutable_put(&seed, b"3:def", 7, Some(5)), IDS, now)
            .unwrap();
        assert_eq!(value_of(&store), (7, Bytes::from_static(b"3:def")));
        assert_eq!(store.bytes(), 5 + 4);
        // No cas at all is fine as well.
        store
            .put(mutable_put(&seed, b"1:e", 8, None), IDS, now)
            .unwrap();
        assert_eq!(value_of(&store), (8, Bytes::from_static(b"1:e")));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn store_expiry() {
        let store = ItemStore::default();
        let start = Instant::now();
        let a = check_put(&put_request(b"1:a", None)).unwrap();
        let b = check_put(&put_request(b"1:b", None)).unwrap();
        store.put(a.clone(), IDS, start).unwrap();
        store.put(b.clone(), IDS, start).unwrap();

        // A put of the same item an hour later refreshes it.
        let later = start + Duration::from_secs(3600);
        store.put(a.clone(), IDS, later).unwrap();

        let after_lifetime = start + ITEM_LIFETIME;
        assert!(store.get(&a.target, after_lifetime).is_some());
        assert_eq!(store.get(&b.target, after_lifetime), None);
        assert!(store.get(&a.target, later + ITEM_LIFETIME).is_none());

        // Expired items are dropped (with their bytes) on the next put.
        let c = check_put(&put_request(b"1:c", None)).unwrap();
        store.put(c, IDS, after_lifetime).unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(store.bytes(), 6);
    }

    /// An immutable put whose target is `target_byte` repeated, as far as the store is
    /// concerned.
    fn item_at(target_byte: u8, len: usize) -> PutItem {
        let mut item = check_put(&put_request(&value_of_len(len), None)).unwrap();
        item.target = Id20::new([target_byte; 20]);
        item
    }

    #[test]
    fn store_bounds_evict_far_items() {
        let now = Instant::now();
        let store = ItemStore::new(3, 1_000_000);
        for b in [0x10, 0x80, 0x20] {
            store.put(item_at(b, 3), IDS, now).unwrap();
        }
        // Full: a closer item pushes out the farthest one.
        store.put(item_at(0x01, 3), IDS, now).unwrap();
        assert_eq!(store.len(), 3);
        assert!(store.get(&Id20::new([0x80; 20]), now).is_none());
        // An item farther than everything we have is refused.
        assert_eq!(
            store.put(item_at(0xf0, 3), IDS, now),
            Err(PutError::StorageFull)
        );
        assert_eq!(PutError::StorageFull.code(), 202);
        assert_eq!(store.len(), 3);

        // The byte limit works the same way.
        let store = ItemStore::new(100, 25);
        store.put(item_at(0x40, 10), IDS, now).unwrap();
        store.put(item_at(0x08, 10), IDS, now).unwrap();
        store.put(item_at(0x04, 10), IDS, now).unwrap();
        assert_eq!(store.bytes(), 20);
        assert!(store.get(&Id20::new([0x40; 20]), now).is_none());
        assert_eq!(
            store.put(item_at(0x50, 10), IDS, now),
            Err(PutError::StorageFull)
        );

        // Distance counts from the closer of our two ids.
        let ids = [Id20::new([0; 20]), Id20::new([0xff; 20])];
        let store = ItemStore::new(2, 1_000_000);
        store.put(item_at(0xfe, 3), ids, now).unwrap();
        store.put(item_at(0x70, 3), ids, now).unwrap();
        store.put(item_at(0x01, 3), ids, now).unwrap();
        assert!(store.get(&Id20::new([0xfe; 20]), now).is_some());
        assert!(store.get(&Id20::new([0x70; 20]), now).is_none());
    }
}
