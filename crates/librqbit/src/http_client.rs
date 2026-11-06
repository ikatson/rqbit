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
