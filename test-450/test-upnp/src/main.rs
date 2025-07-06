use std::time::Duration;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let f = librqbit_upnp::UpnpPortForwarder::new(vec![5678], None, None).unwrap();
    tokio::time::timeout(Duration::from_millis(100), f.run_forever())
        .await
        .unwrap();
}
