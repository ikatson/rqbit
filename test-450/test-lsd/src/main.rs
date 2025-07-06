#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    librqbit_lsd::LocalServiceDiscovery::new(Default::default())
        .await
        .unwrap();
}
