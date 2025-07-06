use librqbit::SessionOptions;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    librqbit::Session::new_with_opts(
        "/tmp/scratch".into(),
        SessionOptions {
            persistence: None,
            disable_dht_persistence: true,
            listen: None,
            ..Default::default()
        },
    )
    .await
    .unwrap();
}
