//! The request / response exchange shared by NAT-PMP and PCP, and the epoch checks both use to
//! notice a gateway that lost its mappings.

use std::{
    fmt,
    mem::MaybeUninit,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use anyhow::Context;
use librqbit_dualstack_sockets::{BindDevice, BindOpts, UdpSocket};
use tokio::time::Instant;
use tracing::trace;

use crate::pcp;

/// When to send a request again. NAT-PMP (RFC 6886 section 3.1): wait 250 ms for the first
/// answer and double the wait after each attempt. PCP (RFC 6887 section 8.1.1): start at 3 s,
/// double up to 1024 s, and spread every wait by up to 10 % either way so that clients do not
/// retransmit in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Retransmit {
    pub initial: Duration,
    /// How many times the request is sent before the gateway counts as silent. The RFC sends it
    /// nine times (about 2 minutes in total).
    pub attempts: u32,
    /// The longest wait between two attempts (PCP's `MRT`).
    pub max_wait: Duration,
    /// Randomise each wait by +-10 % (PCP's `RAND`).
    pub jitter: bool,
}

impl Retransmit {
    /// RFC 6887 section 8.1.1: `MRT`, the cap on the wait between PCP retransmissions.
    pub const PCP_MAX_WAIT: Duration = Duration::from_secs(1024);
}

/// A random number from the OS-seeded hasher of std: good enough to spread retransmissions and
/// to make a PCP nonce hard to guess for someone off the path, without a dependency.
pub(crate) fn random_u64(salt: u64) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(salt);
    h.finish()
}

/// `wait` spread by a random factor between 0.9 and 1.1.
fn jittered(wait: Duration) -> Duration {
    let permille = 900 + u32::try_from(random_u64(0) % 201).unwrap_or(100);
    wait.saturating_mul(permille) / 1000
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

    /// Throws away whatever is queued on the socket: answers that came after their exchange had
    /// already succeeded (to a retransmission, say), or that the gateway sent unasked. NAT-PMP
    /// answers carry no transaction id and a PCP nonce stays the same for a port, so a leftover
    /// answer would otherwise pass for the answer to the next request, with an old epoch and old
    /// results.
    ///
    /// The receive goes straight to the (non-blocking) socket rather than through tokio, whose
    /// readiness may not know yet about a datagram the kernel already holds.
    fn drain(&self) {
        // Bounded, so a flood cannot keep us here; the exchange skips whatever is left.
        const MAX_DRAINED: usize = 64;
        let socket = socket2::SockRef::from(self.socket.socket());
        let mut buf = [MaybeUninit::<u8>::uninit(); RECV_BUF_LEN];
        for _ in 0..MAX_DRAINED {
            match socket.recv(&mut buf) {
                Ok(len) => trace!(gateway = %self.gateway, len, "dropped a stale datagram"),
                // `WouldBlock` once the queue is empty. Any other error (a stale ICMP error)
                // comes back from the exchange itself if it persists.
                Err(_) => return,
            }
        }
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
        self.drain();
        let socket = self.socket.socket();
        let mut buf = [0u8; RECV_BUF_LEN];
        let mut wait = retransmit.initial.min(retransmit.max_wait);
        for _ in 0..retransmit.attempts {
            socket.send(request).await.map_err(ExchangeError::Io)?;
            let this_wait = if retransmit.jitter {
                jittered(wait)
            } else {
                wait
            };
            let deadline = Instant::now() + this_wait;
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
            wait = wait.saturating_mul(2).min(retransmit.max_wait);
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

    #[tokio::test]
    async fn stale_datagrams_are_dropped_before_an_exchange() {
        let server = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let socket = GatewaySocket::connect(server.local_addr().unwrap(), None)
            .await
            .unwrap();
        let client = socket.socket.socket().local_addr().unwrap();

        // A late answer to an earlier request is waiting on the socket.
        server.send_to(b"stale", client).await.unwrap();
        let mut peeked = [0u8; 16];
        socket.socket.socket().peek(&mut peeked).await.unwrap();

        let answer = async {
            let mut buf = [0u8; 16];
            let (len, from) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf[..len], b"request");
            server.send_to(b"fresh", from).await.unwrap();
        };
        let retransmit = Retransmit {
            initial: Duration::from_secs(5),
            attempts: 1,
            max_wait: Duration::MAX,
            jitter: false,
        };
        // Accepts anything: only the drain keeps the stale datagram out.
        let exchange = socket.exchange(b"request", retransmit, |buf| Some(buf.to_vec()));
        let ((), got) = tokio::join!(answer, exchange);
        assert_eq!(got.unwrap(), b"fresh");
    }

    #[test]
    fn jitter_stays_within_ten_percent() {
        let wait = Duration::from_secs(3);
        for _ in 0..1000 {
            let j = jittered(wait);
            assert!(j >= Duration::from_millis(2700) && j <= Duration::from_millis(3300));
        }
    }
}
