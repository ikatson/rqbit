use std::net::SocketAddr;

use futures::{stream::FuturesUnordered, StreamExt};
use log::warn;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{buffers::ByteString, peer_info_reader, torrent_metainfo::TorrentMetaV1Info};

#[derive(Debug)]
pub enum ReadMetainfoResult {
    Found {
        info: TorrentMetaV1Info<ByteString>,
        rx: UnboundedReceiver<SocketAddr>,
        seen: Vec<SocketAddr>,
    },
    ChannelClosed {
        seen: Vec<SocketAddr>,
    },
}

pub async fn read_metainfo_from_peer_receiver(
    peer_id: [u8; 20],
    info_hash: [u8; 20],
    mut addrs: UnboundedReceiver<SocketAddr>,
) -> ReadMetainfoResult {
    let mut seen = Vec::<SocketAddr>::new();
    let first_addr = match addrs.recv().await {
        Some(addr) => addr,
        None => return ReadMetainfoResult::ChannelClosed { seen },
    };
    seen.push(first_addr);

    let mut unordered = FuturesUnordered::new();
    unordered.push(peer_info_reader::read_metainfo_from_peer(
        first_addr, peer_id, info_hash,
    ));

    loop {
        tokio::select! {
            next_addr = addrs.recv() => {
                match next_addr {
                    Some(addr) => {
                        seen.push(addr);
                        unordered.push(peer_info_reader::read_metainfo_from_peer(addr, peer_id, info_hash));
                    },
                    None => return ReadMetainfoResult::ChannelClosed { seen },
                }
            },
            done = unordered.next(), if !unordered.is_empty() => {
                match done {
                    Some(Ok(info)) => return ReadMetainfoResult::Found { info, seen, rx: addrs },
                    Some(Err(e)) => {
                        warn!("error in peer_info_reader::read_metainfo_from_peer: {}", e);
                    },
                    None => unreachable!()
                }
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::info_hash::decode_info_hash;
    use crate::{dht::jsdht::JsDht, peer_id::generate_peer_id};
    use std::sync::Once;

    static LOG_INIT: Once = Once::new();

    fn init_logging() {
        LOG_INIT.call_once(pretty_env_logger::init)
    }

    #[tokio::test]
    async fn read_metainfo_from_dht() {
        init_logging();

        let info_hash = decode_info_hash("9905f844e5d8787ecd5e08fb46b2eb0a42c131d7").unwrap();
        let peer_rx = JsDht::new(info_hash).start_peer_discovery().unwrap();
        let peer_id = generate_peer_id();
        dbg!(read_metainfo_from_peer_receiver(peer_id, info_hash, peer_rx).await);
    }
}
