//! The request / response exchange shared by NAT-PMP and PCP, and the epoch checks both use to
//! notice a gateway that lost its mappings.

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use anyhow::Context;
use librqbit_dualstack_sockets::{BindDevice, BindOpts, UdpSocket};
use tokio::time::Instant;

use crate::pcp;

/// RFC 6886 section 3.1: wait 250 ms for the first answer, double the wait after each attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Retransmit {
    pub initial: Duration,
    /// How many times the request is sent before the gateway counts as silent. The RFC sends it
    /// nine times (about 2 minutes in total).
    pub attempts: u32,
}

#[derive(Debug)]
pub(crate) enum ExchangeError {
    NoAnswer { attempts: u32 },
    Io(std::io::Error),
}

impl fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExchangeError::NoAnswer { attempts } => {
                write!(f, "no answer after {attempts} attempts")
            }
            // An ICMP port unreachable shows up here (as "connection refused"): RFC 6886 says
            // the client may then stop retransmitting at once.
            ExchangeError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ExchangeError {}

/// A UDP socket connected to one gateway, so the kernel drops datagrams from anyone else (RFC
/// 6887 section 8.3 wants answers from other addresses or ports ignored).
pub(crate) struct GatewaySocket {
    socket: UdpSocket,
    gateway: SocketAddr,
    local_ip: IpAddr,
}

/// Big enough to see that a datagram is longer than the largest valid PCP message (1100 bytes)
/// so the decoders can reject it.
const RECV_BUF_LEN: usize = pcp::MAX_MESSAGE_LEN + 4;

impl GatewaySocket {
    pub async fn connect(
        gateway: SocketAddr,
        bind_device: Option<&BindDevice>,
    ) -> anyhow::Result<Self> {
        let bind_addr: SocketAddr = match gateway {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let socket = UdpSocket::bind_udp(
            bind_addr,
            BindOpts {
                request_dualstack: false,
                device: bind_device,
                ..Default::default()
            },
        )
        .with_context(|| format!("error binding a UDP socket to talk to {gateway}"))?;
        socket
            .socket()
            .connect(gateway)
            .await
            .with_context(|| format!("error connecting a UDP socket to {gateway}"))?;
        let local_ip = socket
            .socket()
            .local_addr()
            .context("error reading the local address of a UDP socket")?
            .ip();
        Ok(Self {
            socket,
            gateway,
            local_ip,
        })
    }

    pub fn gateway(&self) -> SocketAddr {
        self.gateway
    }

    /// Our address as the gateway sees it: the source address of what we send. PCP puts it in
    /// every request.
    pub fn local_ip(&self) -> IpAddr {
        self.local_ip
    }

    /// Sends `request` and waits for a datagram that `accept` recognises as its answer,
    /// retransmitting on the schedule. Datagrams `accept` turns down (late answers to earlier
    /// requests, garbage) are skipped without restarting the timer.
    pub async fn exchange<T>(
        &self,
        request: &[u8],
        retransmit: Retransmit,
        mut accept: impl FnMut(&[u8]) -> Option<T>,
    ) -> Result<T, ExchangeError> {
        let socket = self.socket.socket();
        let mut buf = [0u8; RECV_BUF_LEN];
        let mut wait = retransmit.initial;
        for _ in 0..retransmit.attempts {
            socket.send(request).await.map_err(ExchangeError::Io)?;
            let deadline = Instant::now() + wait;
            loop {
                match tokio::time::timeout_at(deadline, socket.recv(&mut buf)).await {
                    Err(_elapsed) => break,
                    Ok(Err(e)) => return Err(ExchangeError::Io(e)),
                    Ok(Ok(len)) => {
                        if let Some(answer) = buf.get(..len).and_then(&mut accept) {
                            return Ok(answer);
                        }
                    }
                }
            }
            wait = wait.saturating_mul(2);
        }
        Err(ExchangeError::NoAnswer {
            attempts: retransmit.attempts,
        })
    }
}

/// Which rule decides that the gateway's epoch went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EpochRule {
    NatPmp,
    Pcp,
}

/// Tracks the "seconds since start of epoch" of one gateway. A gateway that restarts (or loses
/// its mapping table) starts counting again, and both RFCs tell the client to recreate all its
/// mappings at once when it sees that.
#[derive(Debug)]
pub(crate) struct EpochTracker {
    /// How long one gateway second lasts here: one second except in tests.
    second: Duration,
    last: Option<(Instant, u32)>,
}

impl EpochTracker {
    pub fn new(second: Duration) -> Self {
        Self { second, last: None }
    }

    /// Forgets the previous observation: the next one is taken as it is. Used when the protocol
    /// spoken to the gateway changes.
    pub fn reset(&mut self) {
        self.last = None;
    }

    /// Records an epoch seen at `now` and returns `true` when the gateway lost its state since
    /// the previous one.
    pub fn observe(&mut self, rule: EpochRule, now: Instant, server: u32) -> bool {
        let lost = match self.last {
            None => false,
            Some((prev_client, prev_server)) => {
                let elapsed = now.saturating_duration_since(prev_client);
                let client_delta =
                    u64::try_from(elapsed.as_nanos() / self.second.as_nanos().max(1))
                        .unwrap_or(u64::MAX);
                let (server, prev_server) = (u64::from(server), u64::from(prev_server));
                match rule {
                    // RFC 6886 section 3.6: expect at least 7/8 of the time that passed here to
                    // have passed there too; less by more than 2 seconds means a reboot.
                    EpochRule::NatPmp => {
                        let expected =
                            prev_server.saturating_add(client_delta.saturating_mul(7) / 8);
                        server.saturating_add(2) < expected
                    }
                    // RFC 6887 section 8.5.
                    EpochRule::Pcp => {
                        if server.saturating_add(1) < prev_server {
                            true
                        } else {
                            let server_delta = server.saturating_sub(prev_server);
                            client_delta.saturating_add(2) < server_delta - server_delta / 16
                                || server_delta.saturating_add(2) < client_delta - client_delta / 16
                        }
                    }
                }
            }
        };
        self.last = Some((now, server));
        lost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn test_natpmp_epoch() {
        let t0 = Instant::now();
        let mut e = EpochTracker::new(SECOND);
        assert!(!e.observe(EpochRule::NatPmp, t0, 1000));
        // 80 s later the gateway counted 80 s: fine.
        assert!(!e.observe(EpochRule::NatPmp, t0 + 80 * SECOND, 1080));
        // 80 s later it counted 68 s: 7/8 of 80 is 70, 68 is within the 2 s of slack.
        assert!(!e.observe(EpochRule::NatPmp, t0 + 160 * SECOND, 1148));
        // 80 s later it counted 60 s: more than 2 s short, so it rebooted.
        assert!(e.observe(EpochRule::NatPmp, t0 + 240 * SECOND, 1208));
        // A fresh epoch after a reboot.
        assert!(e.observe(EpochRule::NatPmp, t0 + 250 * SECOND, 3));
        // A gateway counting faster than us is not a reboot for NAT-PMP.
        assert!(!e.observe(EpochRule::NatPmp, t0 + 251 * SECOND, 500));
    }

    #[test]
    fn test_pcp_epoch() {
        let t0 = Instant::now();
        let mut e = EpochTracker::new(SECOND);
        assert!(!e.observe(EpochRule::Pcp, t0, 1000));
        // Going back by one second is reordering, not a reboot.
        assert!(!e.observe(EpochRule::Pcp, t0, 999));
        // Going back by more is.
        assert!(e.observe(EpochRule::Pcp, t0, 997));
        // 160 s here, 160 s there.
        assert!(!e.observe(EpochRule::Pcp, t0 + 160 * SECOND, 1157));
        // 160 s here, 151 s there: within 1/16 + 2.
        assert!(!e.observe(EpochRule::Pcp, t0 + 320 * SECOND, 1308));
        // 160 s here, 140 s there: too slow.
        assert!(e.observe(EpochRule::Pcp, t0 + 480 * SECOND, 1448));
        // 10 s here, 40 s there: too fast.
        assert!(e.observe(EpochRule::Pcp, t0 + 490 * SECOND, 1488));
    }

    #[test]
    fn test_epoch_scaled_second() {
        let t0 = Instant::now();
        let mut e = EpochTracker::new(Duration::from_millis(10));
        assert!(!e.observe(EpochRule::Pcp, t0, 0));
        assert!(!e.observe(EpochRule::Pcp, t0 + Duration::from_millis(500), 50));
        e.reset();
        assert!(!e.observe(EpochRule::Pcp, t0 + Duration::from_millis(500), 0));
    }
}
