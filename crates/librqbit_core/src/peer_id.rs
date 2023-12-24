use crate::hash_id::Id20;

#[derive(Debug)]
pub enum AzureusStyleKind {
    Deluge,
    LibTorrent,
    Transmission,
    Other([char; 2]),
}

#[derive(Debug)]
pub struct AzureusStyle {
    pub kind: AzureusStyleKind,
    pub version: [char; 4],
}

impl AzureusStyleKind {
    pub const fn from_bytes(b1: u8, b2: u8) -> Self {
        match &[b1, b2] {
            b"DE" => AzureusStyleKind::Deluge,
            b"lt" | b"LT" => AzureusStyleKind::LibTorrent,
            b"TR" => AzureusStyleKind::Transmission,
            _ => AzureusStyleKind::Other([b1 as char, b2 as char]),
        }
    }
}

fn try_decode_azureus_style(p: &Id20) -> Option<AzureusStyle> {
    let p = p.0;
    if !(p[0] == b'-' && p[7] == b'-') {
        return None;
    }
    let mut version = ['0'; 4];
    for (i, c) in p[3..7].iter().copied().enumerate() {
        version[i] = c as char;
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

pub fn generate_peer_id() -> Id20 {
    let mut peer_id = [0u8; 20];

    let u = uuid::Uuid::new_v4();
    peer_id[4..20].copy_from_slice(&u.as_bytes()[..]);

    peer_id[..8].copy_from_slice(b"-rQ0001-");

    Id20::new(peer_id)
}
