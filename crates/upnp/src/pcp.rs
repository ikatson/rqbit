//! Port Control Protocol messages (RFC 6887): the common header (section 7) and the MAP opcode
//! (section 11.1). PEER and ANNOUNCE are not used.

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

use crate::PortMappingProtocol;

pub(crate) const VERSION: u8 = 2;
pub(crate) const OP_MAP: u8 = 1;
const RESPONSE_BIT: u8 = 0x80;

const HEADER_LEN: usize = 24;
const MAP_PAYLOAD_LEN: usize = 36;
pub(crate) const MAP_LEN: usize = HEADER_LEN + MAP_PAYLOAD_LEN;
/// Section 7: "All PCP messages are sent over UDP, with a maximum UDP payload length of 1100
/// octets."
pub(crate) const MAX_MESSAGE_LEN: usize = 1100;

pub(crate) const NONCE_LEN: usize = 12;
pub(crate) type Nonce = [u8; NONCE_LEN];

/// IANA protocol numbers, as the MAP opcode wants them.
fn protocol_number(protocol: PortMappingProtocol) -> u8 {
    match protocol {
        PortMappingProtocol::Tcp => 6,
        PortMappingProtocol::Udp => 17,
    }
}

fn protocol_from_number(n: u8) -> Option<PortMappingProtocol> {
    match n {
        6 => Some(PortMappingProtocol::Tcp),
        17 => Some(PortMappingProtocol::Udp),
        _ => None,
    }
}

/// Section 5: IPv4 addresses travel as IPv4-mapped IPv6 addresses, IPv6 ones as they are.
fn encode_ip(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped().octets(),
        IpAddr::V6(v6) => v6.octets(),
    }
}

/// The inverse of [`encode_ip`]. `to_ipv4_mapped` checks all of the first 96 bits, as section 5
/// requires.
fn decode_ip(bytes: [u8; 16]) -> IpAddr {
    let v6 = Ipv6Addr::from(bytes);
    match v6.to_ipv4_mapped() {
        Some(v4) => IpAddr::V4(v4),
        None => IpAddr::V6(v6),
    }
}

/// The all-zeros address of the family of `like` (section 5 spells out both forms).
fn unspecified_like(like: IpAddr) -> IpAddr {
    match like {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    }
}

fn be_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn be_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn array<const N: usize>(b: &[u8], at: usize) -> Option<[u8; N]> {
    b.get(at..at + N)?.try_into().ok()
}

/// A MAP request (figure 2 + figure 9). A lifetime of 0 deletes the mapping; section 15.1 then
/// wants the suggested external address and port zeroed, which [`MapRequest::delete`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MapRequest {
    pub lifetime: u32,
    /// Our own address as the gateway sees it: the source address of the request.
    pub client_ip: IpAddr,
    pub nonce: Nonce,
    pub protocol: PortMappingProtocol,
    pub internal_port: u16,
    pub suggested_external_port: u16,
    /// `None` means "no preference", sent as the all-zeros address of the client's family.
    pub suggested_external_ip: Option<IpAddr>,
}

impl MapRequest {
    pub fn delete(
        client_ip: IpAddr,
        nonce: Nonce,
        protocol: PortMappingProtocol,
        internal_port: u16,
    ) -> Self {
        MapRequest {
            lifetime: 0,
            client_ip,
            nonce,
            protocol,
            internal_port,
            suggested_external_port: 0,
            suggested_external_ip: None,
        }
    }

    pub fn encode(&self) -> [u8; MAP_LEN] {
        let mut buf = [0u8; MAP_LEN];
        buf[0] = VERSION;
        buf[1] = OP_MAP;
        // buf[2..4] reserved.
        buf[4..8].copy_from_slice(&self.lifetime.to_be_bytes());
        buf[8..24].copy_from_slice(&encode_ip(self.client_ip));
        buf[24..36].copy_from_slice(&self.nonce);
        buf[36] = protocol_number(self.protocol);
        // buf[37..40] reserved.
        buf[40..42].copy_from_slice(&self.internal_port.to_be_bytes());
        buf[42..44].copy_from_slice(&self.suggested_external_port.to_be_bytes());
        let suggested = self
            .suggested_external_ip
            .unwrap_or_else(|| unspecified_like(self.client_ip));
        buf[44..60].copy_from_slice(&encode_ip(suggested));
        buf
    }

    /// Parses a MAP request the way a server would. Only the fake gateway of the tests needs it.
    #[cfg(any(test, feature = "test-util"))]
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        check_length(buf, MAP_LEN)?;
        if buf[0] != VERSION {
            return Err(DecodeError::Version(buf[0]));
        }
        if buf[1] & RESPONSE_BIT != 0 {
            return Err(DecodeError::NotARequest);
        }
        if buf[1] != OP_MAP {
            return Err(DecodeError::Opcode(buf[1]));
        }
        let truncated = || DecodeError::Truncated(buf.len());
        let protocol = protocol_from_number(buf[36]).ok_or(DecodeError::Protocol(buf[36]))?;
        let suggested = decode_ip(array(buf, 44).ok_or_else(truncated)?);
        Ok(MapRequest {
            lifetime: be_u32(buf, 4).ok_or_else(truncated)?,
            client_ip: decode_ip(array(buf, 8).ok_or_else(truncated)?),
            nonce: array(buf, 24).ok_or_else(truncated)?,
            protocol,
            internal_port: be_u16(buf, 40).ok_or_else(truncated)?,
            suggested_external_port: be_u16(buf, 42).ok_or_else(truncated)?,
            suggested_external_ip: (!suggested.is_unspecified()).then_some(suggested),
        })
    }
}

/// Result codes of section 7.4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultCode {
    Success,
    UnsuppVersion,
    NotAuthorized,
    MalformedRequest,
    UnsuppOpcode,
    UnsuppOption,
    MalformedOption,
    NetworkFailure,
    NoResources,
    UnsuppProtocol,
    UserExQuota,
    CannotProvideExternal,
    AddressMismatch,
    ExcessiveRemotePeers,
    Other(u8),
}

impl ResultCode {
    pub fn from_u8(code: u8) -> Self {
        match code {
            0 => ResultCode::Success,
            1 => ResultCode::UnsuppVersion,
            2 => ResultCode::NotAuthorized,
            3 => ResultCode::MalformedRequest,
            4 => ResultCode::UnsuppOpcode,
            5 => ResultCode::UnsuppOption,
            6 => ResultCode::MalformedOption,
            7 => ResultCode::NetworkFailure,
            8 => ResultCode::NoResources,
            9 => ResultCode::UnsuppProtocol,
            10 => ResultCode::UserExQuota,
            11 => ResultCode::CannotProvideExternal,
            12 => ResultCode::AddressMismatch,
            13 => ResultCode::ExcessiveRemotePeers,
            other => ResultCode::Other(other),
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            ResultCode::Success => 0,
            ResultCode::UnsuppVersion => 1,
            ResultCode::NotAuthorized => 2,
            ResultCode::MalformedRequest => 3,
            ResultCode::UnsuppOpcode => 4,
            ResultCode::UnsuppOption => 5,
            ResultCode::MalformedOption => 6,
            ResultCode::NetworkFailure => 7,
            ResultCode::NoResources => 8,
            ResultCode::UnsuppProtocol => 9,
            ResultCode::UserExQuota => 10,
            ResultCode::CannotProvideExternal => 11,
            ResultCode::AddressMismatch => 12,
            ResultCode::ExcessiveRemotePeers => 13,
            ResultCode::Other(other) => other,
        }
    }
}

impl fmt::Display for ResultCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            ResultCode::Success => "SUCCESS",
            ResultCode::UnsuppVersion => "UNSUPP_VERSION",
            ResultCode::NotAuthorized => "NOT_AUTHORIZED",
            ResultCode::MalformedRequest => "MALFORMED_REQUEST",
            ResultCode::UnsuppOpcode => "UNSUPP_OPCODE",
            ResultCode::UnsuppOption => "UNSUPP_OPTION",
            ResultCode::MalformedOption => "MALFORMED_OPTION",
            ResultCode::NetworkFailure => "NETWORK_FAILURE",
            ResultCode::NoResources => "NO_RESOURCES",
            ResultCode::UnsuppProtocol => "UNSUPP_PROTOCOL",
            ResultCode::UserExQuota => "USER_EX_QUOTA",
            ResultCode::CannotProvideExternal => "CANNOT_PROVIDE_EXTERNAL",
            ResultCode::AddressMismatch => "ADDRESS_MISMATCH",
            ResultCode::ExcessiveRemotePeers => "EXCESSIVE_REMOTE_PEERS",
            ResultCode::Other(_) => "unknown result code",
        };
        write!(f, "{name} ({})", self.to_u8())
    }
}

/// The MAP opcode data of a response (figure 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MapResponseData {
    pub nonce: Nonce,
    pub protocol: PortMappingProtocol,
    pub internal_port: u16,
    pub assigned_external_port: u16,
    pub assigned_external_ip: IpAddr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Response {
    /// The version the server speaks. Equal to [`VERSION`] except in `UNSUPP_VERSION`
    /// answers, where it carries the version the server would accept (section 9).
    pub version: u8,
    pub opcode: u8,
    pub result: ResultCode,
    /// Mapping lifetime on success; how long the error will last otherwise.
    pub lifetime: u32,
    pub epoch: u32,
    /// Absent only from error answers too short to carry it (an `UNSUPP_VERSION` answer to a
    /// version the server cannot parse, for example).
    pub map: Option<MapResponseData>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeError {
    Truncated(usize),
    Oversized(usize),
    NotMultipleOf4(usize),
    Version(u8),
    #[cfg_attr(not(any(test, feature = "test-util")), allow(dead_code))]
    NotARequest,
    NotAResponse,
    Opcode(u8),
    Protocol(u8),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated(len) => write!(f, "truncated PCP message ({len} bytes)"),
            DecodeError::Oversized(len) => write!(f, "oversized PCP message ({len} bytes)"),
            DecodeError::NotMultipleOf4(len) => {
                write!(f, "PCP message length {len} is not a multiple of 4")
            }
            DecodeError::Version(v) => write!(f, "unexpected PCP version {v}"),
            DecodeError::NotARequest => write!(f, "PCP message is not a request"),
            DecodeError::NotAResponse => write!(f, "PCP message is not a response"),
            DecodeError::Opcode(op) => write!(f, "unexpected PCP opcode {op}"),
            DecodeError::Protocol(p) => write!(f, "unexpected PCP protocol number {p}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Section 8.3: "Responses shorter than 24 octets, longer than 1100 octets, or not a multiple
/// of 4 octets are invalid and ignored." Requests follow the same rules (section 8.2).
fn check_length(buf: &[u8], min: usize) -> Result<(), DecodeError> {
    if buf.len() < min {
        return Err(DecodeError::Truncated(buf.len()));
    }
    if buf.len() > MAX_MESSAGE_LEN {
        return Err(DecodeError::Oversized(buf.len()));
    }
    if !buf.len().is_multiple_of(4) {
        return Err(DecodeError::NotMultipleOf4(buf.len()));
    }
    Ok(())
}

/// Whether `buf` is a PCP server's `UNSUPP_VERSION` answer to a request of any opcode. A PCP-only
/// server answers a NAT-PMP request that way (RFC 6887 appendix A), with whatever opcode the
/// request had, so [`Response::decode`] (which only knows MAP) would turn it down.
pub(crate) fn is_unsupported_version_answer(buf: &[u8]) -> bool {
    check_length(buf, HEADER_LEN).is_ok()
        && buf.get(1).is_some_and(|op| op & RESPONSE_BIT != 0)
        && buf.get(3).copied().map(ResultCode::from_u8) == Some(ResultCode::UnsuppVersion)
}

impl Response {
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        check_length(buf, HEADER_LEN)?;
        let truncated = || DecodeError::Truncated(buf.len());
        let version = buf[0];
        if buf[1] & RESPONSE_BIT == 0 {
            return Err(DecodeError::NotAResponse);
        }
        let opcode = buf[1] & !RESPONSE_BIT;
        let result = ResultCode::from_u8(buf[3]);
        if version != VERSION && result != ResultCode::UnsuppVersion {
            return Err(DecodeError::Version(version));
        }
        if opcode != OP_MAP {
            return Err(DecodeError::Opcode(opcode));
        }
        let lifetime = be_u32(buf, 4).ok_or_else(truncated)?;
        let epoch = be_u32(buf, 8).ok_or_else(truncated)?;
        let map = if buf.len() >= MAP_LEN {
            let protocol = protocol_from_number(buf[36]).ok_or(DecodeError::Protocol(buf[36]))?;
            Some(MapResponseData {
                nonce: array(buf, 24).ok_or_else(truncated)?,
                protocol,
                internal_port: be_u16(buf, 40).ok_or_else(truncated)?,
                assigned_external_port: be_u16(buf, 42).ok_or_else(truncated)?,
                assigned_external_ip: decode_ip(array(buf, 44).ok_or_else(truncated)?),
            })
        } else if result == ResultCode::Success {
            // A successful MAP answer must say what was mapped.
            return Err(DecodeError::Truncated(buf.len()));
        } else {
            None
        };
        // Anything after the MAP data is options, which we never ask for and ignore.
        Ok(Response {
            version,
            opcode,
            result,
            lifetime,
            epoch,
            map,
        })
    }

    /// Serialises a response the way a server would. Only the fake gateway of the tests needs it.
    #[cfg(any(test, feature = "test-util"))]
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = vec![0u8; HEADER_LEN];
        buf[0] = self.version;
        buf[1] = self.opcode | RESPONSE_BIT;
        buf[3] = self.result.to_u8();
        buf[4..8].copy_from_slice(&self.lifetime.to_be_bytes());
        buf[8..12].copy_from_slice(&self.epoch.to_be_bytes());
        if let Some(map) = &self.map {
            buf.extend_from_slice(&map.nonce);
            buf.push(protocol_number(map.protocol));
            buf.extend_from_slice(&[0, 0, 0]);
            buf.extend_from_slice(&map.internal_port.to_be_bytes());
            buf.extend_from_slice(&map.assigned_external_port.to_be_bytes());
            buf.extend_from_slice(&encode_ip(map.assigned_external_ip));
        }
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: Nonce = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

    #[test]
    fn map_request_layout_ipv4() {
        let req = MapRequest {
            lifetime: 7200,
            client_ip: "192.168.1.10".parse().unwrap(),
            nonce: NONCE,
            protocol: PortMappingProtocol::Tcp,
            internal_port: 6881,
            suggested_external_port: 6881,
            suggested_external_ip: None,
        };
        let bytes = req.encode();
        let mut expected = vec![2, 1, 0, 0, 0, 0, 0x1c, 0x20];
        // Client address as an IPv4-mapped IPv6 address.
        expected.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 192, 168, 1, 10]);
        expected.extend_from_slice(&NONCE);
        expected.extend_from_slice(&[6, 0, 0, 0, 0x1a, 0xe1, 0x1a, 0xe1]);
        // "No preference" for IPv4 is ::ffff:0.0.0.0, not ::.
        expected.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 0, 0]);
        assert_eq!(bytes.to_vec(), expected);
        assert_eq!(MapRequest::decode(&bytes), Ok(req));
    }

    #[test]
    fn map_request_layout_ipv6_delete() {
        let client_ip: IpAddr = "2001:db8::10".parse().unwrap();
        let req = MapRequest::delete(client_ip, NONCE, PortMappingProtocol::Udp, 6881);
        let bytes = req.encode();
        assert_eq!(&bytes[..8], &[2, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            &bytes[8..24],
            &"2001:db8::10".parse::<Ipv6Addr>().unwrap().octets()
        );
        assert_eq!(&bytes[36..44], &[17, 0, 0, 0, 0x1a, 0xe1, 0, 0]);
        // "No preference" for IPv6 is ::.
        assert_eq!(&bytes[44..60], &[0u8; 16]);
        assert_eq!(MapRequest::decode(&bytes), Ok(req));
    }

    #[test]
    fn map_response_layout() {
        let mut bytes = vec![2, 0x81, 0, 0, 0, 0, 0x0e, 0x10, 0, 0, 0, 42];
        bytes.extend_from_slice(&[0; 12]);
        bytes.extend_from_slice(&NONCE);
        bytes.extend_from_slice(&[17, 0, 0, 0, 0x1a, 0xe1, 0xc3, 0x50]);
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 203, 0, 113, 7]);
        let resp = Response::decode(&bytes).unwrap();
        let expected = Response {
            version: 2,
            opcode: OP_MAP,
            result: ResultCode::Success,
            lifetime: 3600,
            epoch: 42,
            map: Some(MapResponseData {
                nonce: NONCE,
                protocol: PortMappingProtocol::Udp,
                internal_port: 6881,
                assigned_external_port: 50000,
                assigned_external_ip: "203.0.113.7".parse().unwrap(),
            }),
        };
        assert_eq!(resp, expected);
        assert_eq!(resp.encode(), bytes);

        // Options after the MAP data are ignored.
        let mut with_option = bytes.clone();
        with_option.extend_from_slice(&[0x80, 0, 0, 0]);
        assert_eq!(Response::decode(&with_option), Ok(expected));
    }

    #[test]
    fn ipv6_external_address_is_kept() {
        let resp = Response {
            version: 2,
            opcode: OP_MAP,
            result: ResultCode::Success,
            lifetime: 1,
            epoch: 1,
            map: Some(MapResponseData {
                nonce: NONCE,
                protocol: PortMappingProtocol::Tcp,
                internal_port: 1,
                assigned_external_port: 1,
                assigned_external_ip: "2001:db8::1".parse().unwrap(),
            }),
        };
        assert_eq!(Response::decode(&resp.encode()), Ok(resp));
        // Only ::ffff:0:0/96 is IPv4; ::1.2.3.4 (the deprecated compatible form) stays IPv6.
        let mut compat = [0u8; 16];
        compat[12..].copy_from_slice(&[1, 2, 3, 4]);
        assert!(decode_ip(compat).is_ipv6());
    }

    #[test]
    fn result_codes() {
        for code in 0..=255u8 {
            assert_eq!(ResultCode::from_u8(code).to_u8(), code);
        }
        assert_eq!(ResultCode::from_u8(1), ResultCode::UnsuppVersion);
        assert_eq!(ResultCode::from_u8(2), ResultCode::NotAuthorized);
        assert_eq!(ResultCode::from_u8(8), ResultCode::NoResources);
        assert_eq!(ResultCode::from_u8(13), ResultCode::ExcessiveRemotePeers);
        assert_eq!(ResultCode::from_u8(14), ResultCode::Other(14));
    }

    #[test]
    fn short_error_responses() {
        // An UNSUPP_VERSION answer carrying only the header, offering version 1.
        let mut bytes = vec![1, 0x81, 0, 1, 0, 0, 0x07, 0x08, 0, 0, 0, 3];
        bytes.extend_from_slice(&[0; 12]);
        assert_eq!(
            Response::decode(&bytes),
            Ok(Response {
                version: 1,
                opcode: OP_MAP,
                result: ResultCode::UnsuppVersion,
                lifetime: 1800,
                epoch: 3,
                map: None,
            })
        );
        // A short success is not acceptable.
        bytes[0] = 2;
        bytes[3] = 0;
        assert_eq!(Response::decode(&bytes), Err(DecodeError::Truncated(24)));
        // Another version without UNSUPP_VERSION is not ours.
        bytes[0] = 3;
        assert_eq!(Response::decode(&bytes), Err(DecodeError::Version(3)));
    }

    #[test]
    fn malformed_responses_are_rejected() {
        let good = Response {
            version: 2,
            opcode: OP_MAP,
            result: ResultCode::Success,
            lifetime: 1,
            epoch: 1,
            map: Some(MapResponseData {
                nonce: NONCE,
                protocol: PortMappingProtocol::Tcp,
                internal_port: 1,
                assigned_external_port: 1,
                assigned_external_ip: "198.51.100.1".parse().unwrap(),
            }),
        }
        .encode();
        assert_eq!(
            Response::decode(&good[..20]),
            Err(DecodeError::Truncated(20))
        );
        assert_eq!(
            Response::decode(&good[..59]),
            Err(DecodeError::NotMultipleOf4(59))
        );
        let mut huge = good.clone();
        huge.resize(1104, 0);
        assert_eq!(Response::decode(&huge), Err(DecodeError::Oversized(1104)));
        let mut request = good.clone();
        request[1] = OP_MAP;
        assert_eq!(Response::decode(&request), Err(DecodeError::NotAResponse));
        let mut peer = good.clone();
        peer[1] = 0x82;
        assert_eq!(Response::decode(&peer), Err(DecodeError::Opcode(2)));
        let mut sctp = good.clone();
        sctp[36] = 132;
        assert_eq!(Response::decode(&sctp), Err(DecodeError::Protocol(132)));

        assert_eq!(
            MapRequest::decode(&good[..56]),
            Err(DecodeError::Truncated(56))
        );
        assert_eq!(MapRequest::decode(&good), Err(DecodeError::NotARequest));
    }
}
