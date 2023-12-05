use librqbit_upnp::UpnpPortForwarder;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: {} <port>", args[0]);
        return Ok(());
    }

    let port: u16 = match args[1].parse() {
        Ok(p) => p,
        Err(_) => {
            eprintln!("Invalid port number: {}", args[1]);
            return Ok(());
        }
    };

    let port_forwarder = UpnpPortForwarder::new(vec![port], None)?;

    port_forwarder.run_forever().await;
    Ok(())
}
