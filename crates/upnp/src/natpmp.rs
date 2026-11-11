//! NAT Port Mapping Protocol messages (RFC 6886, section 3).
//!
//! Only the codec lives here; the exchange (retransmission, epoch checks) is in `transport.rs`
//! and the mapping logic in `mapper.rs`.

use std::{fmt, net::Ipv4Addr};

use crate::PortMappingProtocol;

/// NAT-PMP is version 0 of the protocol PCP later took over (PCP is version 2).
pub(crate) const VERSION: u8 = 0;
pub(crate) const OP_PUBLIC_ADDRESS: u8 = 0;
pub(crate) const OP_MAP_UDP: u8 = 1;
pub(crate) const OP_MAP_TCP: u8 = 2;
/// Responses carry the request opcode with the top bit set.
const RESPONSE_BIT: u8 = 0x80;

#[cfg(any(test, feature = "test-util"))]
const PUBLIC_ADDRESS_REQUEST_LEN: usize = 2;
const MAP_REQUEST_LEN: usize = 12;
/// Version, opcode, result code and epoch: what every response starts with.
const RESPONSE_HEADER_LEN: usize = 8;
const PUBLIC_ADDRESS_RESPONSE_LEN: usize = 12;
const MAP_RESPONSE_LEN: usize = 16;

pub(crate) fn map_opcode(protocol: PortMappingProtocol) -> u8 {
    match protocol {
        PortMappingProtocol::Udp => OP_MAP_UDP,
        PortMappingProtocol::Tcp => OP_MAP_TCP,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    PublicAddress,
    /// A mapping request. A lifetime of 0 (with a suggested external port of 0) deletes the
    /// mapping (section 3.4).
    Map {
        protocol: PortMappingProtocol,
        internal_port: u16,
        suggested_external_port: u16,
        lifetime: u32,
    },
}

impl Request {
    #[cfg(any(test, feature = "test-util"))]
    pub fn opcode(&self) -> u8 {
        match self {
            Request::PublicAddress => OP_PUBLIC_ADDRESS,
            Request::Map { protocol, .. } => map_opcode(*protocol),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        match *self {
            Request::PublicAddress => vec![VERSION, OP_PUBLIC_ADDRESS],
            Request::Map {
                protocol,
                internal_port,
                suggested_external_port,
                lifetime,
            } => {
                let mut buf = Vec::with_capacity(MAP_REQUEST_LEN);
                buf.extend_from_slice(&[VERSION, map_opcode(protocol), 0, 0]);
                buf.extend_from_slice(&internal_port.to_be_bytes());
                buf.extend_from_slice(&suggested_external_port.to_be_bytes());
                buf.extend_from_slice(&lifetime.to_be_bytes());
                buf
            }
        }
    }

    /// Parses a request the way a gateway would. Only the fake gateway of the tests needs it.
    #[cfg(any(test, feature = "test-util"))]
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        let (&version, &opcode) = match buf {
            [v, o, ..] => (v, o),
            _ => return Err(DecodeError::Truncated(buf.len())),
        };
        if version != VERSION {
            return Err(DecodeError::Version(version));
        }
        let protocol = match opcode {
            OP_PUBLIC_ADDRESS => {
                return match buf.len() {
                    PUBLIC_ADDRESS_REQUEST_LEN => Ok(Request::PublicAddress),
                    len => Err(DecodeError::Oversized(len)),
                };
            }
            OP_MAP_UDP => PortMappingProtocol::Udp,
            OP_MAP_TCP => PortMappingProtocol::Tcp,
            other => return Err(DecodeError::Opcode(other)),
        };
        let fixed: &[u8; MAP_REQUEST_LEN] = match buf.len() {
            MAP_REQUEST_LEN => buf
                .try_into()
                .map_err(|_| DecodeError::Truncated(buf.len()))?,
            len if len < MAP_REQUEST_LEN => return Err(DecodeError::Truncated(len)),
            len => return Err(DecodeError::Oversized(len)),
        };
        Ok(Request::Map {
            protocol,
            internal_port: u16::from_be_bytes([fixed[4], fixed[5]]),
            suggested_external_port: u16::from_be_bytes([fixed[6], fixed[7]]),
            lifetime: u32::from_be_bytes([fixed[8], fixed[9], fixed[10], fixed[11]]),
        })
    }
}

/// Result codes of section 3.5. Unknown codes are kept: the RFC says they are fatal errors of
/// the request, which is how the mapper treats every code but `Success`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultCode {
    Success,
    UnsupportedVersion,
    NotAuthorized,
    NetworkFailure,
    OutOfResources,
    UnsupportedOpcode,
    Other(u16),
}

impl ResultCode {
    pub fn from_u16(code: u16) -> Self {
        match code {
            0 => ResultCode::Success,
            1 => ResultCode::UnsupportedVersion,
            2 => ResultCode::NotAuthorized,
            3 => ResultCode::NetworkFailure,
            4 => ResultCode::OutOfResources,
            5 => ResultCode::UnsupportedOpcode,
            other => ResultCode::Other(other),
        }
    }

    pub fn to_u16(self) -> u16 {
        match self {
            ResultCode::Success => 0,
            ResultCode::UnsupportedVersion => 1,
            ResultCode::NotAuthorized => 2,
            ResultCode::NetworkFailure => 3,
            ResultCode::OutOfResources => 4,
            ResultCode::UnsupportedOpcode => 5,
            ResultCode::Other(other) => other,
        }
    }
}

impl fmt::Display for ResultCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            ResultCode::Success => "success",
            ResultCode::UnsupportedVersion => "unsupported version",
            ResultCode::NotAuthorized => "not authorized / refused",
            ResultCode::NetworkFailure => "network failure",
            ResultCode::OutOfResources => "out of resources",
            ResultCode::UnsupportedOpcode => "unsupported opcode",
            ResultCode::Other(_) => "unknown result code",
        };
        write!(f, "{name} ({})", self.to_u16())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseBody {
    /// An error response that stops after the epoch. RFC 6886 shows the "unsupported version"
    /// answer in that short form, so any error may come like this.
    None,
    PublicAddress(Ipv4Addr),
    Map {
        internal_port: u16,
        external_port: u16,
        lifetime: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Response {
    /// The opcode of the request this answers (the response bit removed).
    pub opcode: u8,
    pub result: ResultCode,
    /// Seconds since the gateway's port mapping table was (re)initialised.
    pub epoch: u32,
    pub body: ResponseBody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeError {
    Truncated(usize),
    Oversized(usize),
    Version(u8),
    NotAResponse,
    Opcode(u8),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated(len) => write!(f, "truncated NAT-PMP message ({len} bytes)"),
            DecodeError::Oversized(len) => write!(f, "oversized NAT-PMP message ({len} bytes)"),
            DecodeError::Version(v) => write!(f, "not a NAT-PMP message (version {v})"),
            DecodeError::NotAResponse => write!(f, "NAT-PMP message is not a response"),
            DecodeError::Opcode(op) => write!(f, "unknown NAT-PMP opcode {op}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl Response {
    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        let header: &[u8; RESPONSE_HEADER_LEN] = buf
            .get(..RESPONSE_HEADER_LEN)
            .and_then(|h| h.try_into().ok())
            .ok_or(DecodeError::Truncated(buf.len()))?;
        if header[0] != VERSION {
            return Err(DecodeError::Version(header[0]));
        }
        let result = ResultCode::from_u16(u16::from_be_bytes([header[2], header[3]]));
        let epoch = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        let opcode = header[1] & !RESPONSE_BIT;
        // The "unsupported version" answer of section 3.5 is drawn with OP = 0, without the
        // response bit, so it is accepted either way.
        if header[1] & RESPONSE_BIT == 0 && result != ResultCode::UnsupportedVersion {
            return Err(DecodeError::NotAResponse);
        }
        if buf.len() == RESPONSE_HEADER_LEN && result != ResultCode::Success {
            return Ok(Response {
                opcode,
                result,
                epoch,
                body: ResponseBody::None,
            });
        }
        let expected_len = match opcode {
            OP_PUBLIC_ADDRESS => PUBLIC_ADDRESS_RESPONSE_LEN,
            OP_MAP_UDP | OP_MAP_TCP => MAP_RESPONSE_LEN,
            other => return Err(DecodeError::Opcode(other)),
        };
        if buf.len() < expected_len {
            return Err(DecodeError::Truncated(buf.len()));
        }
        if buf.len() > expected_len {
            return Err(DecodeError::Oversized(buf.len()));
        }
        let rest = &buf[RESPONSE_HEADER_LEN..];
        let body = match rest {
            [a, b, c, d] if opcode == OP_PUBLIC_ADDRESS => {
                ResponseBody::PublicAddress(Ipv4Addr::new(*a, *b, *c, *d))
            }
            [ip0, ip1, ep0, ep1, l0, l1, l2, l3] => ResponseBody::Map {
                internal_port: u16::from_be_bytes([*ip0, *ip1]),
                external_port: u16::from_be_bytes([*ep0, *ep1]),
                lifetime: u32::from_be_bytes([*l0, *l1, *l2, *l3]),
            },
            _ => return Err(DecodeError::Truncated(buf.len())),
        };
        Ok(Response {
            opcode,
            result,
            epoch,
            body,
        })
    }

    /// Serialises a response the way a gateway would. Only the fake gateway of the tests needs it.
    #[cfg(any(test, feature = "test-util"))]
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(MAP_RESPONSE_LEN);
        buf.push(VERSION);
        buf.push(self.opcode | RESPONSE_BIT);
        buf.extend_from_slice(&self.result.to_u16().to_be_bytes());
        buf.extend_from_slice(&self.epoch.to_be_bytes());
        match self.body {
            ResponseBody::None => {}
            ResponseBody::PublicAddress(ip) => buf.extend_from_slice(&ip.octets()),
            ResponseBody::Map {
                internal_port,
                external_port,
                lifetime,
            } => {
                buf.extend_from_slice(&internal_port.to_be_bytes());
                buf.extend_from_slice(&external_port.to_be_bytes());
                buf.extend_from_slice(&lifetime.to_be_bytes());
            }
        }
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_address_request_layout() {
        // Section 3.2: version 0, opcode 0, nothing else.
        assert_eq!(Request::PublicAddress.encode(), vec![0, 0]);
        assert_eq!(Request::decode(&[0, 0]), Ok(Request::PublicAddress));
        assert_eq!(Request::decode(&[0, 0, 0]), Err(DecodeError::Oversized(3)));
    }

    #[test]
    fn map_request_layout() {
        // Section 3.3: version, opcode, 16 reserved bits, internal port, suggested external
        // port, lifetime, all big-endian.
        let req = Request::Map {
            protocol: PortMappingProtocol::Tcp,
            internal_port: 6881,
            suggested_external_port: 6882,
            lifetime: 7200,
        };
        let bytes = req.encode();
        assert_eq!(
            bytes,
            vec![0, 2, 0, 0, 0x1a, 0xe1, 0x1a, 0xe2, 0, 0, 0x1c, 0x20]
        );
        assert_eq!(Request::decode(&bytes), Ok(req));

        let udp_delete = Request::Map {
            protocol: PortMappingProtocol::Udp,
            internal_port: 1,
            suggested_external_port: 0,
            lifetime: 0,
        };
        assert_eq!(
            udp_delete.encode(),
            vec![0, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            Request::decode(&udp_delete.encode()[..11]),
            Err(DecodeError::Truncated(11))
        );
        assert_eq!(Request::decode(&[0, 3]), Err(DecodeError::Opcode(3)));
        assert_eq!(Request::decode(&[2, 1]), Err(DecodeError::Version(2)));
    }

    #[test]
    fn public_address_response_layout() {
        let bytes = [0, 128, 0, 0, 0, 0, 0x01, 0x00, 203, 0, 113, 7];
        let resp = Response::decode(&bytes).unwrap();
        assert_eq!(
            resp,
            Response {
                opcode: OP_PUBLIC_ADDRESS,
                result: ResultCode::Success,
                epoch: 256,
                body: ResponseBody::PublicAddress(Ipv4Addr::new(203, 0, 113, 7)),
            }
        );
        assert_eq!(resp.encode(), bytes.to_vec());
    }

    #[test]
    fn map_response_layout() {
        let bytes = [
            0, 130, 0, 0, 0, 0, 0, 5, 0x1a, 0xe1, 0xc3, 0x50, 0, 0, 0x0e, 0x10,
        ];
        let resp = Response::decode(&bytes).unwrap();
        assert_eq!(
            resp,
            Response {
                opcode: OP_MAP_TCP,
                result: ResultCode::Success,
                epoch: 5,
                body: ResponseBody::Map {
                    internal_port: 6881,
                    external_port: 50000,
                    lifetime: 3600,
                },
            }
        );
        assert_eq!(resp.encode(), bytes.to_vec());
    }

    #[test]
    fn result_codes() {
        for (code, expected) in [
            (0, ResultCode::Success),
            (1, ResultCode::UnsupportedVersion),
            (2, ResultCode::NotAuthorized),
            (3, ResultCode::NetworkFailure),
            (4, ResultCode::OutOfResources),
            (5, ResultCode::UnsupportedOpcode),
            (6, ResultCode::Other(6)),
            (0xffff, ResultCode::Other(0xffff)),
        ] {
            assert_eq!(ResultCode::from_u16(code), expected);
            assert_eq!(expected.to_u16(), code);
        }
        let refused = [0, 129, 0, 2, 0, 0, 0, 9, 0x1a, 0xe1, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            Response::decode(&refused).unwrap().result,
            ResultCode::NotAuthorized
        );
    }

    #[test]
    fn short_error_responses() {
        // The "unsupported version" figure of section 3.5 has OP = 0 and stops after the epoch.
        let unsupported = [0, 0, 0, 1, 0, 0, 0, 42];
        assert_eq!(
            Response::decode(&unsupported),
            Ok(Response {
                opcode: 0,
                result: ResultCode::UnsupportedVersion,
                epoch: 42,
                body: ResponseBody::None,
            })
        );
        // A short success is not a valid answer.
        assert_eq!(
            Response::decode(&[0, 128, 0, 0, 0, 0, 0, 1]),
            Err(DecodeError::Truncated(8))
        );
    }

    #[test]
    fn malformed_responses_are_rejected() {
        assert_eq!(Response::decode(&[]), Err(DecodeError::Truncated(0)));
        assert_eq!(
            Response::decode(&[0, 128, 0, 0, 0, 0, 0]),
            Err(DecodeError::Truncated(7))
        );
        // Truncated public address.
        assert_eq!(
            Response::decode(&[0, 128, 0, 0, 0, 0, 0, 0, 1, 2, 3]),
            Err(DecodeError::Truncated(11))
        );
        // Oversized public address and mapping answers.
        assert_eq!(
            Response::decode(&[0, 128, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5]),
            Err(DecodeError::Oversized(13))
        );
        let mut long_map = vec![0, 130, 0, 0, 0, 0, 0, 0];
        long_map.extend_from_slice(&[0; 9]);
        assert_eq!(Response::decode(&long_map), Err(DecodeError::Oversized(17)));
        let mut short_map = vec![0, 130, 0, 0, 0, 0, 0, 0];
        short_map.extend_from_slice(&[0; 7]);
        assert_eq!(
            Response::decode(&short_map),
            Err(DecodeError::Truncated(15))
        );
        // A request echoed back, a PCP message, an unknown opcode.
        assert_eq!(
            Response::decode(&[0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Err(DecodeError::NotAResponse)
        );
        assert_eq!(
            Response::decode(&[2, 129, 0, 0, 0, 0, 0, 0]),
            Err(DecodeError::Version(2))
        );
        assert_eq!(
            Response::decode(&[0, 131, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Err(DecodeError::Opcode(3))
        );
    }
}
