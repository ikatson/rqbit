//! Host-defined policy for native outgoing peer, tracker and DHT sockets.
//!
//! This is not a network sandbox. It does not configure HTTP clients, DNS,
//! incoming connections, local discovery or UPnP. Hosts must configure those
//! independently. Callbacks run on networking tasks and must not block or panic.
use std::{fmt, net::SocketAddr, sync::Arc};

/// A cheap-to-clone, stateful destination check. The default session has no policy.
#[derive(Clone)]
pub struct OutboundPolicy(Arc<dyn Fn(SocketAddr) -> bool + Send + Sync>);

/// A destination was rejected before a native connection or datagram send.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("outbound destination rejected by host policy: {destination}")]
pub struct OutboundPolicyError {
    pub destination: SocketAddr,
}

impl OutboundPolicy {
    /// The callback receives the actual destination; IPv4-mapped IPv6 is
    /// normalized to IPv4 so it cannot bypass an IPv4 rule.
    pub fn new(filter: impl Fn(SocketAddr) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(filter))
    }

    pub fn check(&self, destination: SocketAddr) -> Result<(), OutboundPolicyError> {
        let canonical = match destination {
            SocketAddr::V4(_) => destination,
            SocketAddr::V6(addr) => addr
                .ip()
                .to_ipv4_mapped()
                .map(|ip| SocketAddr::from((ip, addr.port())))
                .unwrap_or(destination),
        };
        if (self.0)(canonical) {
            Ok(())
        } else {
            Err(OutboundPolicyError { destination })
        }
    }
}

impl fmt::Debug for OutboundPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("OutboundPolicy").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn captured_policy_updates_are_shared_and_mapped_addresses_are_canonicalized() {
        let enabled = Arc::new(AtomicBool::new(true));
        let state = enabled.clone();
        let policy = OutboundPolicy::new(move |addr| {
            state.load(Ordering::SeqCst) && !addr.ip().is_loopback()
        });
        let clone = policy.clone();
        assert!(clone.check("8.8.8.8:6881".parse().unwrap()).is_ok());
        for blocked in ["127.0.0.1:6881", "[::1]:6881", "[::ffff:127.0.0.1]:6881"] {
            let addr = blocked.parse().unwrap();
            assert_eq!(clone.check(addr).unwrap_err().destination, addr);
        }
        enabled.store(false, Ordering::SeqCst);
        assert!(clone.check("8.8.8.8:6881".parse().unwrap()).is_err());
    }
}
