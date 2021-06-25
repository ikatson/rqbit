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

fn try_decode_azureus_style(p: &[u8; 20]) -> Option<AzureusStyle> {
    if !(p[0] == b'-' && p[7] == b'-') {
        return None;
    }
    let mut version = ['0'; 4];
    for (i, c) in (&p[3..7]).iter().copied().enumerate() {
        version[i] = c as char;
    }
    let kind = AzureusStyleKind::from_bytes(p[1], p[2]);
    Some(AzureusStyle { kind, version })
}

#[derive(Debug)]
pub enum PeerId {
    AzureusStyle(AzureusStyle),
}

pub fn try_decode_peer_id(p: [u8; 20]) -> Option<PeerId> {
    Some(PeerId::AzureusStyle(try_decode_azureus_style(&p)?))
}
