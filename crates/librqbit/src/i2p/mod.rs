//! I2P through the SAM v3 bridge of a local I2P router.
//!
//! A torrent with a tracker inside I2P (host ending in `.i2p`) is an *I2P torrent*. When the
//! session has [`I2pOptions`], such a torrent announces to its I2P trackers through the bridge
//! and talks to the peers they return over I2P streams. Unless [`I2pOptions::mixed_mode`] is
//! set it uses nothing else: no clearnet tracker, DHT, LSD, PEX, TCP or uTP.
//!
//! The rest of the client knows peers by `SocketAddr`. I2P peers get synthetic addresses in
//! the IPv6 discard prefix `0100::/64` (RFC 6666), handed out by the [`book::AddressBook`] of
//! the session; the [`crate::stream_connect::StreamConnector`] reaches them through the bridge.

use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    time::Duration,
};

use futures::{Stream, StreamExt};
use librqbit_core::i2p::{is_i2p_host, is_synthetic};
use serde::{Deserialize, Serialize};

pub(crate) mod book;
#[cfg(test)]
pub(crate) mod fake_bridge;
#[cfg(test)]
mod fake_bridge_tests;
mod runtime;
pub(crate) mod sam;

pub(crate) use runtime::{I2pRuntime, TrackerTransport};

/// How the session reaches the I2P network: through the SAM v3 bridge of a local I2P router
/// (I2P's own router or i2pd, with SAM enabled).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct I2pOptions {
    /// The SAM bridge of the local I2P router. Default `127.0.0.1:7656`.
    pub sam_addr: SocketAddr,
    /// Let I2P torrents also use clearnet trackers and peers (and the DHT, LSD and PEX), as
    /// other torrents do. An address learned in one network is still never told to the other.
    /// Default false: an I2P torrent stays inside I2P.
    pub mixed_mode: bool,
    /// How many inbound tunnels the router builds for the session. Default 3.
    pub inbound_quantity: u8,
    /// How many outbound tunnels the router builds for the session. Default 3.
    pub outbound_quantity: u8,
    /// How many hops the inbound tunnels have. Default 3.
    pub inbound_length: u8,
    /// How many hops the outbound tunnels have. Default 3.
    pub outbound_length: u8,
}

impl Default for I2pOptions {
    fn default() -> Self {
        Self {
            sam_addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7656)),
            mixed_mode: false,
            inbound_quantity: 3,
            outbound_quantity: 3,
            inbound_length: 3,
            outbound_length: 3,
        }
    }
}

/// The error of an I2P tracker that the session's tracker list gives a torrent without I2P
/// trackers of its own: such a torrent never uses I2P.
pub(crate) const I2P_NOT_AN_I2P_TORRENT: &str =
    "an I2P tracker, but the torrent has no I2P tracker of its own and stays off I2P";

/// Which networks a torrent uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TorrentNet {
    /// The clearnet only: a torrent without I2P trackers, or any torrent of a session without
    /// I2P.
    Clearnet,
    /// An I2P torrent of a session with I2P; with `mixed` the clearnet too.
    I2p { mixed: bool },
}

impl TorrentNet {
    /// The networks of a torrent with these (its own) trackers.
    pub fn new<'a>(
        i2p: Option<&I2pOptions>,
        trackers: impl IntoIterator<Item = &'a url::Url>,
    ) -> Self {
        let Some(i2p) = i2p else {
            return Self::Clearnet;
        };
        if trackers
            .into_iter()
            .any(|u| u.host_str().is_some_and(is_i2p_host))
        {
            Self::I2p {
                mixed: i2p.mixed_mode,
            }
        } else {
            Self::Clearnet
        }
    }

    pub fn uses_i2p(self) -> bool {
        matches!(self, Self::I2p { .. })
    }

    /// Whether the torrent uses the clearnet: trackers, peers, the DHT, LSD.
    pub fn uses_clearnet(self) -> bool {
        !matches!(self, Self::I2p { mixed: false })
    }

    /// Whether the torrent connects to, or takes a connection from, the peer at `addr`.
    pub fn admits(self, addr: &SocketAddr) -> bool {
        if is_synthetic(addr) {
            self.uses_i2p()
        } else {
            self.uses_clearnet()
        }
    }

    /// Whether the torrent exchanges peers (PEX) with the peer at `addr`. Never inside I2P:
    /// PEX carries IP addresses only, and an I2P peer must not learn clearnet ones.
    pub fn pex_with(self, addr: &SocketAddr) -> bool {
        self.uses_clearnet() && !is_synthetic(addr)
    }
}

/// Opening a stream inside I2P takes several round trips through tunnels: the connect and
/// handshake timeouts of I2P peers are at least these.
const MIN_PEER_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const MIN_PEER_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// The connect timeout for the peer at `addr`.
pub(crate) fn peer_connect_timeout(addr: &SocketAddr, timeout: Duration) -> Duration {
    if is_synthetic(addr) {
        timeout.max(MIN_PEER_CONNECT_TIMEOUT)
    } else {
        timeout
    }
}

/// The handshake timeout for the peer at `addr`.
pub(crate) fn peer_handshake_timeout(addr: &SocketAddr, timeout: Duration) -> Duration {
    if is_synthetic(addr) {
        timeout.max(MIN_PEER_HANDSHAKE_TIMEOUT)
    } else {
        timeout
    }
}

/// The peers a clearnet source (the DHT, LSD) gives, without synthetic addresses: whoever
/// sends one is not an I2P peer, and it must not make the torrent dial into I2P.
pub(crate) fn clearnet_peers<S>(peers: S) -> impl Stream<Item = SocketAddr> + Unpin + Send + use<S>
where
    S: Stream<Item = SocketAddr> + Unpin + Send,
{
    peers.filter(|addr| std::future::ready(!is_synthetic(addr)))
}

#[cfg(test)]
mod tests {
    use librqbit_core::i2p::synthetic_addr;

    use super::*;

    fn urls(list: &[&str]) -> Vec<url::Url> {
        list.iter().map(|u| u.parse().unwrap()).collect()
    }

    #[test]
    fn i2p_torrents_are_those_with_an_i2p_tracker() {
        let opts = I2pOptions::default();
        let i2p = urls(&["http://tracker.i2p/a", "http://example.com/announce"]);
        let b32 = urls(&["http://ahsplxkbhemefwvvml7qovzl5a2b5xo5i7lyai7ntdunvcyfdtna.B32.I2P/a"]);
        let clear = urls(&["http://example.com/announce", "udp://i2p.example.com:80"]);
        assert_eq!(
            TorrentNet::new(Some(&opts), &i2p),
            TorrentNet::I2p { mixed: false }
        );
        assert_eq!(
            TorrentNet::new(Some(&opts), &b32),
            TorrentNet::I2p { mixed: false }
        );
        assert_eq!(TorrentNet::new(Some(&opts), &clear), TorrentNet::Clearnet);
        // Without I2P an I2P torrent is an ordinary one, whose I2P trackers report an error.
        assert_eq!(TorrentNet::new(None, &i2p), TorrentNet::Clearnet);
        let mixed = I2pOptions {
            mixed_mode: true,
            ..Default::default()
        };
        assert_eq!(
            TorrentNet::new(Some(&mixed), &i2p),
            TorrentNet::I2p { mixed: true }
        );
    }

    #[test]
    fn admission_and_pex_by_network() {
        let i2p_peer = synthetic_addr(7);
        let clear_peer: SocketAddr = "192.0.2.1:6881".parse().unwrap();
        let v6_peer: SocketAddr = "[2001:db8::1]:6881".parse().unwrap();

        let c = TorrentNet::Clearnet;
        assert!(c.admits(&clear_peer) && c.admits(&v6_peer) && !c.admits(&i2p_peer));
        assert!(c.pex_with(&clear_peer));

        let only = TorrentNet::I2p { mixed: false };
        assert!(only.admits(&i2p_peer) && !only.admits(&clear_peer) && !only.admits(&v6_peer));
        assert!(!only.pex_with(&i2p_peer) && !only.pex_with(&clear_peer));

        let mixed = TorrentNet::I2p { mixed: true };
        assert!(mixed.admits(&i2p_peer) && mixed.admits(&clear_peer));
        assert!(mixed.pex_with(&clear_peer) && !mixed.pex_with(&i2p_peer));
    }

    #[test]
    fn i2p_peers_get_longer_timeouts() {
        let short = Duration::from_secs(5);
        assert_eq!(
            peer_connect_timeout(&synthetic_addr(1), short),
            MIN_PEER_CONNECT_TIMEOUT
        );
        assert_eq!(
            peer_handshake_timeout(&synthetic_addr(1), short),
            MIN_PEER_HANDSHAKE_TIMEOUT
        );
        let clear: SocketAddr = "192.0.2.1:6881".parse().unwrap();
        assert_eq!(peer_connect_timeout(&clear, short), short);
        assert_eq!(peer_handshake_timeout(&clear, short), short);
    }
}
