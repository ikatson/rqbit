use std::{io::BufRead, io::BufReader, net::SocketAddr, process::Stdio, str::FromStr};

use log::info;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

// Collects seen peers for torrent
// Knows if they work or not.
// Informs subscribers of new peers discovered.
//
// Can discover metainfo quickly (limiting concurrency).

pub struct JsDht {
    info_hash: [u8; 20],
}

static NODEJS_DISCOVER_SCRIPT: &str = r#"
const DHT = require('bittorrent-dht')

let dht = new DHT();
let infoHash = process.env["INFOHASH"];

dht.on('peer', function (peer, infoHash, from) {
    console.log(peer.host + ':' + peer.port)
})

dht.lookup(infoHash)
"#;

fn infohash_hex(info_hash: [u8; 20]) -> String {
    hex::encode(info_hash)
}

impl JsDht {
    pub fn new(info_hash: [u8; 20]) -> Self {
        Self { info_hash }
    }
    pub fn start_peer_discovery(self) -> anyhow::Result<UnboundedReceiver<SocketAddr>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || self.discover_peers_and_send(tx).unwrap());
        Ok(rx)
    }
    fn discover_peers_and_send(self, tx: UnboundedSender<SocketAddr>) -> anyhow::Result<()> {
        let mut cmd = std::process::Command::new("node");
        cmd.arg("-e")
            .arg(NODEJS_DISCOVER_SCRIPT)
            .env("NODE_PATH", "/opt/homebrew/lib/node_modules")
            .env("INFOHASH", infohash_hex(self.info_hash))
            .stdout(Stdio::piped());

        info!("Executing {:?}", &cmd);

        let mut child = cmd.spawn()?;

        let stdout = child.stdout.take().unwrap();
        let mut stdout = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            let size = stdout.read_line(&mut line)?;
            if size == 0 {
                anyhow::bail!("node discover process was not supposed to close")
            }
            // Remove newline character;
            line.pop();

            let ipaddr = SocketAddr::from_str(&line)?;
            if tx.send(ipaddr).is_err() {
                anyhow::bail!("receiver closed")
            }
        }
    }
}
