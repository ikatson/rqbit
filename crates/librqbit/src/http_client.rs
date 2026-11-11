//! Single construction point for the HTTP(S) clients librqbit uses for
//! trackers, `.torrent` URLs and IP block/allow lists.
//!
//! With the `rust-tls` feature, reqwest 0.13 verifies certificates through
//! `rustls-platform-verifier`. On Android that verifier calls into the JVM and
//! **panics** unless the embedding app ran
//! `rustls_platform_verifier::android::init_with_env` and bundled its Kotlin
//! component — neither of which a library can guarantee. Every HTTPS request
//! (e.g. an `https://` tracker announce) would then abort the task polling it.
//!
//! On Android we therefore pin the trust store to the bundled Mozilla roots
//! (`webpki-root-certs`), which bypasses the platform verifier entirely.
//! Other platforms keep the platform verifier (system trust store).

/// Returns a `reqwest::ClientBuilder` with librqbit's TLS policy applied.
/// All librqbit HTTP clients MUST be created through this function.
pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    #[allow(clippy::disallowed_methods)] // the one sanctioned construction point
    let builder = reqwest::Client::builder();
    #[cfg(all(target_os = "android", feature = "rust-tls"))]
    let builder = builder.tls_certs_only(
        webpki_root_certs::TLS_SERVER_ROOT_CERTS
            .iter()
            .filter_map(|der| reqwest::Certificate::from_der(der.as_ref()).ok()),
    );
    builder
}

/// The reqwest proxy for a parsed proxy URL: every request of the client goes through it,
/// credentials included. SOCKS4 user ids are not sent (reqwest has no way to).
///
/// reqwest hands the proxy URL to hyper-util, which silently drops a proxy whose URL it can't
/// parse as an [`http::Uri`] and then connects directly. The URL is therefore built here and
/// checked the same way, so such a proxy is an error instead of a direct connection.
pub(crate) fn proxy(config: &librqbit_core::proxy::ProxyConfig) -> anyhow::Result<reqwest::Proxy> {
    use anyhow::{Context, ensure};
    use librqbit_core::proxy::ProxyKind;

    let mut url = reqwest::Url::parse(&config.url_without_credentials())
        .with_context(|| format!("invalid proxy URL {config}"))?;
    if let (ProxyKind::Socks5 | ProxyKind::Socks5h | ProxyKind::Http, Some(user), Some(pass)) =
        (config.kind(), config.username(), config.password())
    {
        // The URL setters leave '%' alone and hyper-util percent-decodes the credentials, so a
        // literal '%' has to be escaped first.
        let set = url
            .set_username(&user.replace('%', "%25"))
            .and_then(|()| url.set_password(Some(&pass.replace('%', "%25"))));
        ensure!(
            set.is_ok(),
            "can't put the credentials of {config} in a URL"
        );
    }
    let uri: http::Uri = url
        .as_str()
        .parse()
        .with_context(|| format!("the HTTP client can't use the proxy {config}"))?;
    let host = uri
        .host()
        .map(|h| h.trim_start_matches('[').trim_end_matches(']'));
    ensure!(
        host == Some(config.host()),
        "the HTTP client can't use the proxy {config}"
    );
    reqwest::Proxy::all(url)
        .with_context(|| format!("error creating the HTTP client proxy for {config}"))
}
