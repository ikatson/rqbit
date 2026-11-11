//! BEP 44 (storing arbitrary data in the DHT): the targets and signatures of immutable and
//! mutable items, the checks of an incoming `put`, and the store that keeps what other nodes put
//! to us.
//!
//! <https://www.bittorrent.org/beps/bep_0044.html>

use std::{
    cmp::Reverse,
    collections::{HashMap, hash_map::Entry},
    net::IpAddr,
    time::{Duration, Instant},
};

use bencode::ByteBufOwned;
use bytes::Bytes;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use librqbit_core::hash_id::Id20;
use parking_lot::Mutex;
use sha1w::{ISha1, Sha1};

use crate::{bep42::voter_network, bprotocol::PutRequest};

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

/// Whether `signature` is the signature of `public_key` over `salt`, `seq` and `value`. The
/// check is the strict one: it refuses small-order keys, under which a forged signature would
/// verify for any value.
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
    key.verify_strict(
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

// The deepest nesting of lists and dictionaries accepted inside `v`, the same as the bencode
// crate accepts in a whole message.
const MAX_VALUE_DEPTH: usize = 128;

/// Whether `value` is exactly one bencoded value in canonical form, as `v` must be: integers
/// and string lengths without leading zeros (and no `-0`), dictionary keys that are strings in
/// strictly increasing order. Items are hashed and signed byte for byte, so BEP 44 has nodes
/// refuse a value that would bencode differently once parsed.
pub(crate) fn is_canonical_bencode(value: &[u8]) -> bool {
    canonical_value_end(value, 0, 0) == Some(value.len())
}

/// Where the canonical bencoded value starting at `pos` ends, or `None` if there is none.
fn canonical_value_end(buf: &[u8], pos: usize, depth: usize) -> Option<usize> {
    match *buf.get(pos)? {
        b'i' => {
            let start = pos + 1;
            let len = buf.get(start..)?.iter().position(|&c| c == b'e')?;
            let digits = buf.get(start..start + len)?;
            let magnitude = digits.strip_prefix(b"-").unwrap_or(digits);
            let canonical = match magnitude {
                [] => false,
                [b'0'] => digits.len() == 1,
                [first, ..] => *first != b'0' && magnitude.iter().all(u8::is_ascii_digit),
            };
            canonical.then_some(start + len + 1)
        }
        b'0'..=b'9' => canonical_string(buf, pos).map(|(_, end)| end),
        b'l' => {
            if depth >= MAX_VALUE_DEPTH {
                return None;
            }
            let mut pos = pos + 1;
            while *buf.get(pos)? != b'e' {
                pos = canonical_value_end(buf, pos, depth + 1)?;
            }
            Some(pos + 1)
        }
        b'd' => {
            if depth >= MAX_VALUE_DEPTH {
                return None;
            }
            let mut pos = pos + 1;
            let mut previous_key: Option<&[u8]> = None;
            while *buf.get(pos)? != b'e' {
                let (key, key_end) = canonical_string(buf, pos)?;
                if previous_key.is_some_and(|previous| previous >= key) {
                    return None;
                }
                previous_key = Some(key);
                pos = canonical_value_end(buf, key_end, depth + 1)?;
            }
            Some(pos + 1)
        }
        _ => None,
    }
}

/// The canonical bencoded string starting at `pos`, and where it ends.
fn canonical_string(buf: &[u8], pos: usize) -> Option<(&[u8], usize)> {
    let colon = pos + buf.get(pos..)?.iter().position(|&c| c == b':')?;
    let digits = buf.get(pos..colon)?;
    let canonical = match digits {
        [] => false,
        [b'0'] => true,
        [first, ..] => *first != b'0' && digits.iter().all(u8::is_ascii_digit),
    };
    if !canonical {
        return None;
    }
    let len: usize = std::str::from_utf8(digits).ok()?.parse().ok()?;
    let end = colon.checked_add(1)?.checked_add(len)?;
    Some((buf.get(colon + 1..end)?, end))
}

/// A mutable item as found in the DHT. Together with the salt it was looked up with, it has
/// everything needed to check it and to re-publish it (see
/// [`crate::DhtState::put_mutable_signed`]).
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
    /// 202: we have no room for the item and every item we have is worth more to us.
    StorageFull,
    /// 202: the network the put comes from has its share of the store already.
    QuotaExceeded,
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
    /// 302: the item we have has the same sequence number and another value.
    SequenceNotNewer,
}

impl PutError {
    pub(crate) fn code(&self) -> i32 {
        match self {
            PutError::StorageFull | PutError::QuotaExceeded => 202,
            PutError::Protocol(_) => 203,
            PutError::ValueTooBig => 205,
            PutError::InvalidSignature => 206,
            PutError::SaltTooBig => 207,
            PutError::CasMismatch => 301,
            PutError::SequenceTooOld | PutError::SequenceNotNewer => 302,
        }
    }

    pub(crate) fn message(&self) -> &'static str {
        match self {
            PutError::StorageFull => "storage full",
            PutError::QuotaExceeded => "storage full for the network of the sender",
            PutError::Protocol(msg) => msg,
            PutError::ValueTooBig => "message (v field) too big",
            PutError::InvalidSignature => "invalid signature",
            PutError::SaltTooBig => "salt (salt field) too big",
            PutError::CasMismatch => "the CAS hash mismatched, re-read value and try again",
            PutError::SequenceTooOld => "sequence number less than current",
            PutError::SequenceNotNewer => {
                "sequence number equal to current, with a different value"
            }
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

/// The checks of a put that need nothing but the put itself and the token: the fields a
/// mutable put needs, sizes, the bencoding of `v`, and the signature. `token_is_valid` is asked
/// about the target of the put first: it is cheap, and it stops anyone who did not `get` that
/// target from us from making us verify signatures.
pub(crate) fn check_put(
    req: &PutRequest<ByteBufOwned>,
    token_is_valid: impl FnOnce(&Id20) -> bool,
) -> Result<PutItem, PutError> {
    let value = req.v.0.0.clone();
    let mutable = match (&req.k, &req.sig, req.seq) {
        (None, None, _) => None,
        (Some(k), Some(sig), Some(seq)) => Some((k.0, sig.0, seq)),
        _ => {
            return Err(PutError::Protocol(
                "a mutable put needs \"k\", \"sig\" and \"seq\"",
            ));
        }
    };
    let salt = match (&mutable, &req.salt) {
        (Some(_), Some(salt)) => salt.0.clone(),
        _ => Bytes::new(),
    };
    let target = match &mutable {
        None => immutable_target(&value),
        Some((k, ..)) => mutable_target(k, &salt),
    };
    if !token_is_valid(&target) {
        return Err(PutError::Protocol("invalid token"));
    }
    if value.len() > MAX_VALUE_SIZE {
        return Err(PutError::ValueTooBig);
    }
    if salt.len() > MAX_SALT_SIZE {
        return Err(PutError::SaltTooBig);
    }
    if !is_canonical_bencode(&value) {
        return Err(PutError::Protocol("invalid bencoding in v"));
    }
    let Some((k, sig, seq)) = mutable else {
        return Ok(PutItem {
            target,
            value,
            mutable: None,
        });
    };
    if seq < 0 {
        return Err(PutError::Protocol("invalid (negative) sequence number"));
    }
    if !verify_mutable(&k, &salt, seq, &value, &sig) {
        return Err(PutError::InvalidSignature);
    }
    Ok(PutItem {
        target,
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

/// How often the secret behind BEP 44 write tokens changes. A token is good until the second
/// change after it was handed out: 5 to 10 minutes, as in libtorrent.
const TOKEN_SECRET_LIFETIME: Duration = Duration::from_secs(5 * 60);
/// The length of a BEP 44 write token.
const TOKEN_LEN: usize = 8;

struct TokenSecrets {
    current: [u8; 16],
    previous: [u8; 16],
    changed: Instant,
}

/// The write tokens of BEP 44 `get` answers: a hash of a secret, the address of the node that
/// asked and the target it asked about. A token is only good for that target, from that
/// address, for a few minutes, so that one `get` cannot be turned into any number of puts.
/// Nothing is remembered per token (the scheme of libtorrent).
pub(crate) struct WriteTokens {
    secrets: Mutex<TokenSecrets>,
}

impl WriteTokens {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            secrets: Mutex::new(TokenSecrets {
                current: rand::random(),
                previous: rand::random(),
                changed: now,
            }),
        }
    }

    /// The current and the previous secret, changed first if they are due.
    fn secrets(&self, now: Instant) -> ([u8; 16], [u8; 16]) {
        let mut s = self.secrets.lock();
        let elapsed = now.saturating_duration_since(s.changed);
        if elapsed >= 2 * TOKEN_SECRET_LIFETIME {
            s.previous = rand::random();
            s.current = rand::random();
            s.changed = now;
        } else if elapsed >= TOKEN_SECRET_LIFETIME {
            // Counted from when the change was due, not from now, so that a token never
            // outlives two lifetimes of the secret it was made with.
            s.previous = s.current;
            s.current = rand::random();
            s.changed += TOKEN_SECRET_LIFETIME;
        }
        (s.current, s.previous)
    }

    /// The token for a put of `target` from `ip`.
    pub(crate) fn token(&self, ip: IpAddr, target: &Id20, now: Instant) -> [u8; TOKEN_LEN] {
        write_token(&self.secrets(now).0, ip, target)
    }

    /// Whether `token` is one we handed out recently to `ip` for `target`.
    pub(crate) fn is_valid(&self, token: &[u8], ip: IpAddr, target: &Id20, now: Instant) -> bool {
        let (current, previous) = self.secrets(now);
        token == write_token(&current, ip, target) || token == write_token(&previous, ip, target)
    }
}

fn write_token(secret: &[u8; 16], ip: IpAddr, target: &Id20) -> [u8; TOKEN_LEN] {
    let mut h = Sha1::new();
    h.update(secret);
    match ip.to_canonical() {
        IpAddr::V4(ip) => h.update(&ip.octets()),
        IpAddr::V6(ip) => h.update(&ip.octets()),
    }
    h.update(&target.0);
    let digest = h.finish();
    let mut token = [0u8; TOKEN_LEN];
    token.copy_from_slice(&digest[..TOKEN_LEN]);
    token
}

struct MutableMeta {
    public_key: [u8; 32],
    signature: [u8; 64],
    seq: i64,
    salt: Bytes,
}

/// A network (see [`voter_network`]) that put items to us.
type Network = [u8; 8];

// How many of the networks that put an item we remember. An item put from many networks is
// wanted by many and is the last to go when the store is full.
const MAX_ANNOUNCERS: usize = 16;
// One network may own (be the first to put) at most this share of the items and the bytes of
// the store, so that one /24 or /48 cannot fill it and keep everybody else out.
const NETWORK_SHARE: usize = 16;

struct StoredItem {
    value: Bytes,
    mutable: Option<MutableMeta>,
    last_put: Instant,
    /// The network that first put the item: the item counts against its share.
    owner: Network,
    /// The networks that put the item, at most `MAX_ANNOUNCERS`.
    announcers: Vec<Network>,
}

impl StoredItem {
    fn size(&self) -> usize {
        self.value.len() + self.mutable.as_ref().map(|m| m.salt.len()).unwrap_or(0)
    }

    fn is_expired(&self, now: Instant) -> bool {
        now.duration_since(self.last_put) >= ITEM_LIFETIME
    }

    fn refresh(&mut self, network: Network, now: Instant) {
        self.last_put = now;
        if self.announcers.len() < MAX_ANNOUNCERS && !self.announcers.contains(&network) {
            self.announcers.push(network);
        }
    }
}

/// How much an item is worth keeping when the store is full: put from more networks first, then
/// closer to us. The least worth goes first.
type Worth = (usize, Reverse<Id20>);

/// What we have stored under a target, as `get` answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StoredView {
    Immutable(Bytes),
    Mutable(MutableItem),
}

/// What the items a network owns take up.
#[derive(Default, Clone, Copy)]
struct Usage {
    items: usize,
    bytes: usize,
}

#[derive(Default)]
struct StoreInner {
    items: HashMap<Id20, StoredItem>,
    bytes: usize,
    by_owner: HashMap<Network, Usage>,
}

impl StoreInner {
    fn remove(&mut self, target: &Id20) {
        let Some(item) = self.items.remove(target) else {
            return;
        };
        let size = item.size();
        self.bytes -= size;
        if let Entry::Occupied(mut usage) = self.by_owner.entry(item.owner) {
            let u = usage.get_mut();
            u.items = u.items.saturating_sub(1);
            u.bytes = u.bytes.saturating_sub(size);
            if u.items == 0 {
                usage.remove();
            }
        }
    }

    fn expire(&mut self, now: Instant) {
        let expired: Vec<Id20> = self
            .items
            .iter()
            .filter(|(_, item)| item.is_expired(now))
            .map(|(target, _)| *target)
            .collect();
        for target in expired {
            self.remove(&target);
        }
    }
}

/// How far `target` is from us: the distance to the closer of our node ids.
fn distance_from_us(target: &Id20, ids: &[Id20; 2]) -> Id20 {
    target.distance(&ids[0]).min(target.distance(&ids[1]))
}

/// The items other nodes put to us. Bounded in number and bytes, overall and per network that
/// puts them; items expire [`ITEM_LIFETIME`] after their last put. Nothing is persisted.
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

    /// Stores (or refreshes) a checked item put from `source`. `ids` are our node ids. When the
    /// store is full, the items worth least (see [`Worth`]) make room, as long as they are worth
    /// less than the new item; otherwise the new item is refused and nothing is evicted.
    pub(crate) fn put(
        &self,
        item: PutItem,
        source: IpAddr,
        ids: [Id20; 2],
        now: Instant,
    ) -> Result<(), PutError> {
        let network = voter_network(source);
        let mut inner = self.inner.lock();
        inner.expire(now);

        let new_size = item.value.len() + item.mutable.as_ref().map(|m| m.salt.len()).unwrap_or(0);
        // For a new version of a mutable item we have: its owner, size and announcers.
        let existing = match inner.items.get_mut(&item.target) {
            Some(existing) => {
                match (&item.mutable, &existing.mutable) {
                    // Same target, same value: a refresh.
                    (None, None) => {
                        existing.refresh(network, now);
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
                        // The same version again is a refresh. Another value under the same
                        // sequence number is refused: a writer has to move to a new one.
                        if new.seq == old.seq {
                            if existing.value != item.value {
                                return Err(PutError::SequenceNotNewer);
                            }
                            existing.refresh(network, now);
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
                let mut announcers = existing.announcers.len();
                if announcers < MAX_ANNOUNCERS && !existing.announcers.contains(&network) {
                    announcers += 1;
                }
                Some((existing.owner, existing.size(), announcers))
            }
            None => None,
        };
        let (owner, old_size, added_items, announcers) = match existing {
            Some((owner, size, announcers)) => (owner, size, 0, announcers),
            None => (network, 0, 1, 1),
        };

        // The share of the owner of the item.
        let usage = inner.by_owner.get(&owner).copied().unwrap_or_default();
        if usage.items + added_items > self.max_items.div_ceil(NETWORK_SHARE)
            || usage.bytes.saturating_sub(old_size) + new_size
                > self.max_bytes.div_ceil(NETWORK_SHARE)
        {
            return Err(PutError::QuotaExceeded);
        }

        // Room: pick the items to evict first, and evict them only if they make enough.
        let fits = |items: usize, bytes: usize| items <= self.max_items && bytes <= self.max_bytes;
        let mut items_after = inner.items.len() + added_items;
        let mut bytes_after = inner.bytes - old_size + new_size;
        if !fits(items_after, bytes_after) {
            let worth: Worth = (announcers, Reverse(distance_from_us(&item.target, &ids)));
            let mut candidates: Vec<(Worth, Id20, usize)> = inner
                .items
                .iter()
                .filter(|(target, _)| **target != item.target)
                .map(|(target, stored)| {
                    (
                        (
                            stored.announcers.len(),
                            Reverse(distance_from_us(target, &ids)),
                        ),
                        *target,
                        stored.size(),
                    )
                })
                .filter(|(w, ..)| *w < worth)
                .collect();
            candidates.sort_unstable_by_key(|(w, ..)| *w);
            let mut victims = Vec::new();
            for (_, target, size) in candidates {
                if fits(items_after, bytes_after) {
                    break;
                }
                items_after -= 1;
                bytes_after -= size;
                victims.push(target);
            }
            if !fits(items_after, bytes_after) {
                return Err(PutError::StorageFull);
            }
            for target in victims {
                inner.remove(&target);
            }
        }

        let mutable = item.mutable.map(|m| MutableMeta {
            public_key: m.public_key,
            signature: m.signature,
            seq: m.seq,
            salt: m.salt,
        });
        match inner.items.entry(item.target) {
            Entry::Occupied(mut occupied) => {
                let stored = occupied.get_mut();
                stored.value = item.value;
                stored.mutable = mutable;
                stored.refresh(network, now);
            }
            Entry::Vacant(vacant) => {
                vacant.insert(StoredItem {
                    value: item.value,
                    mutable,
                    last_put: now,
                    owner,
                    announcers: vec![network],
                });
            }
        }
        inner.bytes = inner.bytes - old_size + new_size;
        let usage = inner.by_owner.entry(owner).or_default();
        usage.items += added_items;
        usage.bytes = usage.bytes.saturating_sub(old_size) + new_size;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{net::Ipv4Addr, str::FromStr};

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
    fn canonical_bencode() {
        for ok in [
            &b"12:Hello World!"[..],
            b"0:",
            b"i0e",
            b"i-1e",
            b"i42e",
            b"le",
            b"de",
            b"li1ei2ee",
            b"d1:ai1e1:bl1:xee",
            b"d1:a1:x2:aa1:ye",
        ] {
            assert!(is_canonical_bencode(ok), "{}", String::from_utf8_lossy(ok));
        }
        for bad in [
            &b""[..],
            b"i1ei2e",
            b"12:Hello",
            b"i01e",
            b"i-0e",
            b"ie",
            b"i-e",
            b"i1xe",
            b"01:a",
            b"1a:x",
            b"l",
            b"d1:ae",
            // Keys out of order, repeated, or not strings.
            b"d1:b1:x1:a1:ye",
            b"d1:a1:x1:a1:ye",
            b"di1e1:xe",
            b"x",
        ] {
            assert!(
                !is_canonical_bencode(bad),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let nested = |depth: usize| {
            let mut v = vec![b'l'; depth];
            v.extend(std::iter::repeat_n(b'e', depth));
            v
        };
        assert!(is_canonical_bencode(&nested(MAX_VALUE_DEPTH)));
        assert!(!is_canonical_bencode(&nested(MAX_VALUE_DEPTH + 1)));
    }

    #[test]
    fn small_order_keys_are_refused() {
        // The identity point: with it, the all-zero-but-one signature below verifies for any
        // message under the lenient check.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        let mut sig = [0u8; 64];
        sig[0] = 1;
        assert!(!verify_mutable(&identity, b"", 1, VALUE, &sig));
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

    /// [`check_put`] with a token that is good.
    fn check(req: &PutRequest<ByteBufOwned>) -> Result<PutItem, PutError> {
        check_put(req, |_| true)
    }

    /// A bencoded byte string that is `len` bytes long in total, header included.
    fn value_of_len(len: usize) -> Vec<u8> {
        let body = (1..len)
            .find(|n| n.to_string().len() + 1 + n == len)
            .unwrap();
        let mut v = format!("{body}:").into_bytes();
        v.resize(len, b'x');
        assert!(is_canonical_bencode(&v));
        v
    }

    #[test]
    fn check_put_errors() {
        let seed = [3u8; 32];

        let ok = check(&put_request(VALUE, None)).unwrap();
        assert_eq!(ok.target, immutable_target(VALUE));
        assert!(ok.mutable.is_none());

        let ok = check(&put_request(VALUE, Some((&seed, b"s", 5, Some(4))))).unwrap();
        assert_eq!(ok.target, mutable_target(&public_key(&seed), b"s"));
        assert_eq!(ok.mutable.as_ref().unwrap().cas, Some(4));

        // The limits themselves are fine.
        let max = value_of_len(MAX_VALUE_SIZE);
        check(&put_request(&max, None)).unwrap();
        check(&put_request(
            &max,
            Some((&seed, &[b's'; MAX_SALT_SIZE], 1, None)),
        ))
        .unwrap();

        let too_big = value_of_len(MAX_VALUE_SIZE + 1);
        assert_eq!(check(&put_request(&too_big, None)).unwrap_err().code(), 205);
        assert_eq!(
            check(&put_request(&too_big, Some((&seed, b"", 1, None))))
                .unwrap_err()
                .code(),
            205
        );
        assert_eq!(
            check(&put_request(
                VALUE,
                Some((&seed, &[b's'; MAX_SALT_SIZE + 1], 1, None))
            ))
            .unwrap_err(),
            PutError::SaltTooBig
        );
        assert_eq!(
            check(&put_request(VALUE, Some((&seed, b"", -1, None))))
                .unwrap_err()
                .code(),
            203
        );
        // A "v" that would bencode differently once parsed.
        assert_eq!(
            check(&put_request(b"d1:b1:x1:a1:ye", None))
                .unwrap_err()
                .code(),
            203
        );

        // A mutable put without one of its fields.
        let mut incomplete = put_request(VALUE, Some((&seed, b"", 1, None)));
        incomplete.sig = None;
        assert_eq!(check(&incomplete).unwrap_err().code(), 203);

        // A signature that does not match.
        let mut forged = put_request(VALUE, Some((&seed, b"", 1, None)));
        forged.seq = Some(2);
        assert_eq!(check(&forged).unwrap_err(), PutError::InvalidSignature);
        assert_eq!(PutError::InvalidSignature.code(), 206);
        let mut other_key = put_request(VALUE, Some((&seed, b"", 1, None)));
        other_key.k = Some(FixedBytes(public_key(&[4u8; 32])));
        assert_eq!(check(&other_key).unwrap_err().code(), 206);

        // A bad token is refused before anything else, and is asked about the target of the
        // put (for a mutable item: of its key and salt).
        let mut asked = None;
        assert_eq!(
            check_put(&forged, |target| {
                asked = Some(*target);
                false
            })
            .unwrap_err(),
            PutError::Protocol("invalid token")
        );
        assert_eq!(asked, Some(mutable_target(&public_key(&seed), b"")));
        assert_eq!(
            check_put(&put_request(&too_big, None), |_| false)
                .unwrap_err()
                .code(),
            203
        );
    }

    #[test]
    fn write_tokens() {
        let start = Instant::now();
        let tokens = WriteTokens::new(start);
        let ip = IpAddr::from([10, 0, 0, 1]);
        let target = Id20::new([5; 20]);
        let token = tokens.token(ip, &target, start);
        assert!(tokens.is_valid(&token, ip, &target, start));
        // Only from that address, for that target.
        assert!(!tokens.is_valid(&token, IpAddr::from([10, 0, 0, 2]), &target, start));
        assert!(!tokens.is_valid(&token, ip, &Id20::new([6; 20]), start));
        assert!(!tokens.is_valid(&token[..4], ip, &target, start));
        // An IPv4-mapped address is the same node.
        let mapped = IpAddr::V6(Ipv4Addr::new(10, 0, 0, 1).to_ipv6_mapped());
        assert!(tokens.is_valid(&token, mapped, &target, start));

        // Good across one change of the secret, not across two.
        let later = start + TOKEN_SECRET_LIFETIME + Duration::from_secs(1);
        assert!(tokens.is_valid(&token, ip, &target, later));
        let newer = tokens.token(ip, &target, later);
        assert_ne!(newer, token);
        let much_later = start + 2 * TOKEN_SECRET_LIFETIME;
        assert!(!tokens.is_valid(&token, ip, &target, much_later));
        assert!(tokens.is_valid(&newer, ip, &target, much_later));

        // After a long quiet time nothing old is good any more.
        let idle = much_later + 2 * TOKEN_SECRET_LIFETIME;
        assert!(!tokens.is_valid(&newer, ip, &target, idle));
    }

    fn mutable_put(seed: &[u8; 32], v: &[u8], seq: i64, cas: Option<i64>) -> PutItem {
        check(&put_request(v, Some((seed, b"salt", seq, cas)))).unwrap()
    }

    const IDS: [Id20; 2] = [
        librqbit_core::hash_id::Id([0; 20]),
        librqbit_core::hash_id::Id([0; 20]),
    ];

    /// An address in network `n`: every `n` is its own /24.
    fn net(n: u8) -> IpAddr {
        IpAddr::from([10, 0, n, 1])
    }

    #[test]
    fn store_get_immutable_and_mutable() {
        let store = ItemStore::default();
        let now = Instant::now();
        let imm = check(&put_request(VALUE, None)).unwrap();
        store.put(imm.clone(), net(1), IDS, now).unwrap();
        assert_eq!(
            store.get(&imm.target, now),
            Some(StoredView::Immutable(Bytes::from_static(VALUE)))
        );

        let seed = [9u8; 32];
        let m = mutable_put(&seed, b"1:a", 1, None);
        store.put(m.clone(), net(1), IDS, now).unwrap();
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

        // An immutable item under the target of a mutable one is refused.
        let mut clash = imm;
        clash.target = m.target;
        assert_eq!(store.put(clash, net(1), IDS, now).unwrap_err().code(), 203);
    }

    #[test]
    fn store_seq_and_cas() {
        let store = ItemStore::default();
        let now = Instant::now();
        let seed = [9u8; 32];
        let first = mutable_put(&seed, b"1:a", 5, None);
        let target = first.target;
        store.put(first.clone(), net(1), IDS, now).unwrap();

        let value_of = |store: &ItemStore| match store.get(&target, now) {
            Some(StoredView::Mutable(item)) => (item.seq, item.value),
            other => panic!("unexpected {other:?}"),
        };

        // Going backwards is refused.
        assert_eq!(
            store.put(mutable_put(&seed, b"1:b", 4, None), net(1), IDS, now),
            Err(PutError::SequenceTooOld)
        );
        assert_eq!(PutError::SequenceTooOld.code(), 302);
        // The same version again refreshes it; another value under the same seq is refused.
        store.put(first, net(2), IDS, now).unwrap();
        assert_eq!(
            store.put(mutable_put(&seed, b"1:c", 5, None), net(1), IDS, now),
            Err(PutError::SequenceNotNewer)
        );
        assert_eq!(PutError::SequenceNotNewer.code(), 302);
        assert_eq!(value_of(&store), (5, Bytes::from_static(b"1:a")));
        // A cas that does not match the current seq is refused, even with a newer seq.
        assert_eq!(
            store.put(mutable_put(&seed, b"1:d", 7, Some(4)), net(1), IDS, now),
            Err(PutError::CasMismatch)
        );
        assert_eq!(PutError::CasMismatch.code(), 301);
        assert_eq!(value_of(&store), (5, Bytes::from_static(b"1:a")));
        // A matching cas with a newer seq replaces the item.
        store
            .put(mutable_put(&seed, b"3:def", 7, Some(5)), net(1), IDS, now)
            .unwrap();
        assert_eq!(value_of(&store), (7, Bytes::from_static(b"3:def")));
        assert_eq!(store.bytes(), 5 + 4);
        // No cas at all is fine as well.
        store
            .put(mutable_put(&seed, b"1:e", 8, None), net(1), IDS, now)
            .unwrap();
        assert_eq!(value_of(&store), (8, Bytes::from_static(b"1:e")));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn store_expiry() {
        let store = ItemStore::default();
        let start = Instant::now();
        let a = check(&put_request(b"1:a", None)).unwrap();
        let b = check(&put_request(b"1:b", None)).unwrap();
        store.put(a.clone(), net(1), IDS, start).unwrap();
        store.put(b.clone(), net(1), IDS, start).unwrap();

        // A put of the same item an hour later refreshes it.
        let later = start + Duration::from_secs(3600);
        store.put(a.clone(), net(2), IDS, later).unwrap();

        let after_lifetime = start + ITEM_LIFETIME;
        assert!(store.get(&a.target, after_lifetime).is_some());
        assert_eq!(store.get(&b.target, after_lifetime), None);
        assert!(store.get(&a.target, later + ITEM_LIFETIME).is_none());

        // Expired items are dropped (with their bytes) on the next put.
        let c = check(&put_request(b"1:c", None)).unwrap();
        store.put(c, net(1), IDS, after_lifetime).unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(store.bytes(), 6);
    }

    /// An immutable put of `len` bytes whose target is `target_byte` repeated, as far as the
    /// store is concerned.
    fn item_at(target_byte: u8, len: usize) -> PutItem {
        let mut item = check(&put_request(&value_of_len(len), None)).unwrap();
        item.target = Id20::new([target_byte; 20]);
        item
    }

    #[test]
    fn store_bounds_evict_least_worth() {
        let now = Instant::now();
        let store = ItemStore::new(3, 1_000_000);
        for (n, b) in [0x10, 0x80, 0x20].into_iter().enumerate() {
            store.put(item_at(b, 3), net(n as u8), IDS, now).unwrap();
        }
        // Full: a closer item pushes out the farthest one.
        store.put(item_at(0x01, 3), net(3), IDS, now).unwrap();
        assert_eq!(store.len(), 3);
        assert!(store.get(&Id20::new([0x80; 20]), now).is_none());
        // An item farther than everything we have is refused.
        assert_eq!(
            store.put(item_at(0xf0, 3), net(4), IDS, now),
            Err(PutError::StorageFull)
        );
        assert_eq!(PutError::StorageFull.code(), 202);
        assert_eq!(store.len(), 3);

        // An item put from two networks outlives closer items put from one.
        store.put(item_at(0x20, 3), net(5), IDS, now).unwrap();
        store.put(item_at(0x02, 3), net(6), IDS, now).unwrap();
        assert!(store.get(&Id20::new([0x10; 20]), now).is_none());
        assert!(store.get(&Id20::new([0x20; 20]), now).is_some());
        assert_eq!(
            store.put(item_at(0x03, 3), net(7), IDS, now),
            Err(PutError::StorageFull)
        );

        // The byte limit works the same way, and an item that needs several to go evicts them
        // all, or none when it is not worth more than all of them.
        let store = ItemStore::new(100, 320);
        for n in 0..32u8 {
            store.put(item_at(0x40 + n, 10), net(n), IDS, now).unwrap();
        }
        assert_eq!(store.bytes(), 320);
        store.put(item_at(0x01, 20), net(40), IDS, now).unwrap();
        assert_eq!(store.bytes(), 320);
        assert_eq!(store.len(), 31);
        assert!(store.get(&Id20::new([0x5f; 20]), now).is_none());
        assert!(store.get(&Id20::new([0x5e; 20]), now).is_none());
        let mut between = item_at(0, 20);
        between.target = Id20::new([
            0x5d, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        assert_eq!(
            store.put(between, net(41), IDS, now),
            Err(PutError::StorageFull)
        );
        assert!(store.get(&Id20::new([0x5d; 20]), now).is_some());
        assert_eq!(store.len(), 31);

        // Distance counts from the closer of our two ids.
        let ids = [Id20::new([0; 20]), Id20::new([0xff; 20])];
        let store = ItemStore::new(2, 1_000_000);
        store.put(item_at(0xfe, 3), net(1), ids, now).unwrap();
        store.put(item_at(0x70, 3), net(2), ids, now).unwrap();
        store.put(item_at(0x01, 3), net(3), ids, now).unwrap();
        assert!(store.get(&Id20::new([0xfe; 20]), now).is_some());
        assert!(store.get(&Id20::new([0x70; 20]), now).is_none());
    }

    #[test]
    fn store_network_share() {
        let now = Instant::now();
        // A share of 2 items per network.
        let store = ItemStore::new(32, 1_000_000);
        store.put(item_at(0x01, 3), net(1), IDS, now).unwrap();
        // Another address in the same /24 is the same network.
        store
            .put(item_at(0x02, 3), IpAddr::from([10, 0, 1, 200]), IDS, now)
            .unwrap();
        assert_eq!(
            store.put(item_at(0x03, 3), net(1), IDS, now),
            Err(PutError::QuotaExceeded)
        );
        assert_eq!(PutError::QuotaExceeded.code(), 202);
        // Others still have room, and the network may still refresh what it owns.
        store.put(item_at(0x03, 3), net(2), IDS, now).unwrap();
        store.put(item_at(0x01, 3), net(1), IDS, now).unwrap();
        // Once its items expire, it has its share back.
        let later = now + ITEM_LIFETIME;
        store.put(item_at(0x04, 3), net(1), IDS, later).unwrap();
        store.put(item_at(0x05, 3), net(1), IDS, later).unwrap();

        // A share of 100 bytes per network.
        let store = ItemStore::new(10_000, 1600);
        for b in 0..10 {
            store.put(item_at(b, 10), net(1), IDS, now).unwrap();
        }
        assert_eq!(
            store.put(item_at(0x10, 10), net(1), IDS, now),
            Err(PutError::QuotaExceeded)
        );
        // A new version of a mutable item counts against the network that first put it.
        let seed = [1u8; 32];
        store
            .put(mutable_put(&seed, b"1:a", 1, None), net(2), IDS, now)
            .unwrap();
        let big = value_of_len(96);
        store
            .put(mutable_put(&seed, &big, 2, None), net(1), IDS, now)
            .unwrap();
        let too_big = value_of_len(97);
        assert_eq!(
            store.put(mutable_put(&seed, &too_big, 3, None), net(3), IDS, now),
            Err(PutError::QuotaExceeded)
        );
    }
}
