use crate::hash_id::Id20;
use rand::{self, RngCore};

/// Return the version of the invoking crate as a tuple
#[macro_export]
macro_rules! crate_version {
    () => {
        (
            env!("CARGO_PKG_VERSION_MAJOR").parse::<u8>().unwrap_or(0),
            env!("CARGO_PKG_VERSION_MINOR").parse::<u8>().unwrap_or(0),
            env!("CARGO_PKG_VERSION_PATCH").parse::<u8>().unwrap_or(0),
            env!("CARGO_PKG_VERSION_PRE").parse::<u8>().unwrap_or(0),
        )
    };
}

#[derive(Debug)]
pub enum AzureusStyleKind {
    Deluge,
    LibTorrent,
    Transmission,
    QBittorrent,
    UTorrent,
    RQBit,
    Other([u8; 2]),
}

#[derive(Debug)]
pub struct AzureusStyle {
    pub kind: AzureusStyleKind,
    pub version: [u8; 4],
}

impl AzureusStyleKind {
    pub const fn from_bytes(b1: u8, b2: u8) -> Self {
        match &[b1, b2] {
            b"DE" => AzureusStyleKind::Deluge,
            b"lt" | b"LT" => AzureusStyleKind::LibTorrent,
            b"TR" => AzureusStyleKind::Transmission,
            b"qB" => AzureusStyleKind::QBittorrent,
            b"UT" => AzureusStyleKind::UTorrent,
            b"rQ" => AzureusStyleKind::RQBit,
            other => AzureusStyleKind::Other(*other),
        }
    }
}

fn try_decode_azureus_style(p: &Id20) -> Option<AzureusStyle> {
    let p = p.0;
    if !(p[0] == b'-' && p[7] == b'-') {
        return None;
    }
    let mut version = [b'0'; 4];
    for (i, c) in p[3..7].iter().copied().enumerate() {
        version[i] = version_digit_from_id(c)?;
    }
    let kind = AzureusStyleKind::from_bytes(p[1], p[2]);
    Some(AzureusStyle { kind, version })
}

#[derive(Debug)]
pub enum PeerId {
    AzureusStyle(AzureusStyle),
}

pub fn try_decode_peer_id(p: Id20) -> Option<PeerId> {
    Some(PeerId::AzureusStyle(try_decode_azureus_style(&p)?))
}

/// Returns `None` for bytes greater than 64
fn version_digit_to_id(d: u8) -> Option<u8> {
    let version_map = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz.-";
    version_map.get(d as usize).copied()
}

/// Returns `None` for bytes that aren't alphanumeric, `.` or `-`.
fn version_digit_from_id(d: u8) -> Option<u8> {
    match d {
        b'0'..=b'9' => Some(d - b'0'),
        b'A'..=b'Z' => Some(d - b'0' - 10),
        b'a'..=b'z' => Some(d - b'0' - 10 - 26),
        b'.' => Some(62),
        b'-' => Some(63),
        _ => None,
    }
}

/// Generate a client fingerprint in the Azereus format, where `b"-xx1234-"` corresponds to version `1.2.3.4`` of the torrent client abbreviated by `xx`
pub fn generate_azereus_style(client: [u8; 2], version: (u8, u8, u8, u8)) -> Id20 {
    let mut fingerprint = [b'-'; 8];

    fingerprint[1..3].copy_from_slice(&client);
    fingerprint[3] = version_digit_to_id(version.0).unwrap();
    fingerprint[4] = version_digit_to_id(version.1).unwrap();
    fingerprint[5] = version_digit_to_id(version.2).unwrap();
    fingerprint[6] = version_digit_to_id(version.3).unwrap();
    generate_peer_id(&fingerprint)
}

/// Panics if the `fingerprint` slice isn't eight bytes long
pub fn generate_peer_id(fingerprint: &[u8]) -> Id20 {
    let mut peer_id = [0u8; 20];

    peer_id[..8].copy_from_slice(fingerprint);
    rand::rng().fill_bytes(&mut peer_id[8..]);

    Id20::new(peer_id)
}

#[cfg(test)]
mod tests {
    use crate::peer_id::generate_azereus_style;

    #[test]
    fn test_azereus_peer_id_generation() {
        for (client, version, correct_fingerprint) in [
            (*b"xx", (1, 2, 3, 4), *b"-xx1234-"),
            (*b"00", (10, 0, 0, 0), *b"-00A000-"),
            (*b"\xFF\xFF", (36, 37, 62, 63), *b"-\xFF\xFFab.--"),
        ] {
            let id1 = generate_azereus_style(client, version);
            let id2 = generate_azereus_style(client, version);
            assert_ne!(id1, id2);
            assert_eq!(id1.0[..8], id2.0[..8]);
            assert_eq!(id1.0[..8], correct_fingerprint);
        }
    }
}
