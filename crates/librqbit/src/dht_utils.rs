use std::{collections::HashSet, net::SocketAddr};

use anyhow::Context;
use buffers::ByteString;
use futures::{stream::FuturesUnordered, Stream, StreamExt};
use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
use tracing::debug;

use crate::{
    peer_connection::PeerConnectionOptions, peer_info_reader, spawn_utils::BlockingSpawner,
};
use librqbit_core::hash_id::Id20;

#[derive(Debug)]
pub enum ReadMetainfoResult<Rx> {
    Found {
        info: TorrentMetaV1Info<ByteString>,
        rx: Rx,
        seen: HashSet<SocketAddr>,
    },
    ChannelClosed {
        seen: HashSet<SocketAddr>,
    },
}

pub async fn read_metainfo_from_peer_receiver<A: Stream<Item = SocketAddr> + Unpin>(
    peer_id: Id20,
    info_hash: Id20,
    initial_addrs: Vec<SocketAddr>,
    addrs_stream: A,
    peer_connection_options: Option<PeerConnectionOptions>,
) -> ReadMetainfoResult<A> {
    let mut seen = HashSet::<SocketAddr>::new();
    let mut addrs = addrs_stream;

    let semaphore = tokio::sync::Semaphore::new(128);

    let read_info_guarded = |addr| {
        let semaphore = &semaphore;
        async move {
            let token = semaphore.acquire().await?;
            let ret = peer_info_reader::read_metainfo_from_peer(
                addr,
                peer_id,
                info_hash,
                peer_connection_options,
                BlockingSpawner::new(true),
            )
            .await
            .with_context(|| format!("error reading metainfo from {addr}"));
            drop(token);
            ret
        }
    };

    let mut unordered = FuturesUnordered::new();

    for a in initial_addrs {
        seen.insert(a);
        unordered.push(read_info_guarded(a));
    }

    loop {
        tokio::select! {
            next_addr = addrs.next() => {
                match next_addr {
                    Some(addr) => {
                        if seen.insert(addr) {
                            unordered.push(read_info_guarded(addr));
                        }
                    },
                    None => return ReadMetainfoResult::ChannelClosed { seen },
                }
            },
            done = unordered.next(), if !unordered.is_empty() => {
                match done {
                    Some(Ok(info)) => return ReadMetainfoResult::Found { info, seen, rx: addrs },
                    Some(Err(e)) => {
                        debug!("{:#}", e);
                    },
                    None => unreachable!()
                }
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use dht::{DhtBuilder, Id20};
    use librqbit_core::peer_id::generate_peer_id;

    use super::*;
    use std::{str::FromStr, sync::Once};

    static LOG_INIT: Once = Once::new();

    fn init_logging() {
        #[allow(unused_must_use)]
        LOG_INIT.call_once(|| {
            // pretty_env_logger::try_init();
        })
    }

    #[tokio::test]
    #[ignore]
    async fn read_metainfo_from_dht() {
        init_logging();

        let info_hash = Id20::from_str("cab507494d02ebb1178b38f2e9d7be299c86b862").unwrap();
        let dht = DhtBuilder::new().await.unwrap();

        let peer_rx = dht.get_peers(info_hash, None).unwrap();
        let peer_id = generate_peer_id();
        match read_metainfo_from_peer_receiver(peer_id, info_hash, Vec::new(), peer_rx, None).await
        {
            ReadMetainfoResult::Found { info, .. } => dbg!(info),
            ReadMetainfoResult::ChannelClosed { .. } => todo!("should not have happened"),
        };
    }
}
