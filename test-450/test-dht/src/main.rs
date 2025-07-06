use std::time::Duration;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    librqbit_dht::DhtBuilder::new().await.unwrap();
}
