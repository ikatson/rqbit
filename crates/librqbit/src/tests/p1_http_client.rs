use std::time::Duration;

use tokio::{net::TcpListener, time::timeout};

use crate::{
    AddTorrent, Session, SessionOptions,
    tests::test_util::{install_crypto_provider, setup_test_logging},
};

// SessionOptions.http_client must be used verbatim: a client injected by the
// caller has to carry the caller's proxy/TLS configuration. Point the injected
// client at a local listener acting as proxy and check that librqbit's HTTP
// traffic ends up there.
#[tokio::test(flavor = "multi_thread")]
async fn injected_http_client_is_used() {
    setup_test_logging();
    install_crypto_provider();
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy.local_addr().unwrap();

    let proxied = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{proxy_addr}")).unwrap())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    let dir = tempfile::TempDir::with_prefix("rqbit_p1_http_client").unwrap();
    let session = Session::new_with_opts(
        dir.path().into(),
        SessionOptions {
            dht: None,
            persistence: None,
            disable_trackers: true,
            disable_local_service_discovery: true,
            http_client: Some(proxied),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let proxied_hit = tokio::spawn(async move {
        timeout(Duration::from_secs(10), proxy.accept())
            .await
            .expect("no connection to the injected proxy")
            .map(|(stream, _)| drop(stream))
            .unwrap()
    });

    // The fetch itself is expected to fail - nothing speaks proxy protocol on
    // the other end, so the connection is dropped right after the accept.
    // What matters is that the request went through the injected client.
    let _ = session
        .add_torrent(
            AddTorrent::Url("http://torrent.invalid/x.torrent".into()),
            None,
        )
        .await;
    proxied_hit.await.unwrap();
}