mod tracker_comms;
mod tracker_comms_http;
mod tracker_comms_udp;

pub use tracker_comms::*;
pub use tracker_comms_udp::UdpTrackerClient;

/// Build a reqwest client that works with whichever TLS backend is enabled.
///
/// With `rust-tls-ring` (ring provider via `reqwest/rustls-no-provider`),
/// reqwest has no baked-in crypto provider, so install the ring provider as
/// the process default before building the client. Otherwise
/// `Client::builder().build()` panics. Installing twice (e.g. from multiple
/// call sites) is fine: the loser of the race is ignored.
pub fn build_reqwest_client(builder: reqwest::ClientBuilder) -> anyhow::Result<reqwest::Client> {
    #[cfg(feature = "rust-tls-ring")]
    {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_build_reqwest_client_does_not_panic() {
        // Regression test: with `rustls-no-provider` (via librqbit/rust-tls),
        // reqwest has no baked-in crypto provider. Building a client without
        // an installed process-default provider panics. The helper above
        // installs the ring provider first when `rust-tls-ring` is on.
        let _client = super::build_reqwest_client(reqwest::Client::builder())
            .expect("building reqwest client must succeed");
    }
}
