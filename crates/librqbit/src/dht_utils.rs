use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use buffers::ByteBufOwned;
use futures::{Stream, StreamExt, stream::FuturesUnordered};
use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
use tracing::{Instrument, debug, debug_span};

use crate::{
    peer_connection::PeerConnectionOptions, peer_info_reader, spawn_utils::BlockingSpawner,
    stream_connect::StreamConnector,
};
use librqbit_core::hash_id::Id20;

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ReadMetainfoResult<Rx> {
    Found {
        info: TorrentMetaV1Info<ByteBufOwned>,
        info_bytes: ByteBufOwned,
        rx: Rx,
        seen: HashSet<SocketAddr>,
    },
    ChannelClosed {
        #[allow(dead_code)]
        seen: HashSet<SocketAddr>,
    },
}

pub async fn read_metainfo_from_peer_receiver<A: Stream<Item = SocketAddr> + Unpin>(
    peer_id: Id20,
    info_hash: Id20,
    initial_addrs: Vec<SocketAddr>,
    addrs_stream: A,
    peer_connection_options: Option<PeerConnectionOptions>,
    connector: Arc<StreamConnector>,
    client_name_and_version: String,
) -> ReadMetainfoResult<A> {
    let mut seen = HashSet::<SocketAddr>::new();
    let mut addrs = addrs_stream;

    let semaphore = tokio::sync::Semaphore::new(128);

    let read_info_guarded = |addr| {
        let semaphore = &semaphore;
        let connector = connector.clone();
        let client_name_and_version = client_name_and_version.clone();
        async move {
            let token = match semaphore.acquire().await {
                Ok(token) => token,
                Err(e) => return (addr, Err(e.into())),
            };
            let ret = peer_info_reader::read_metainfo_from_peer(
                addr,
                peer_id,
                info_hash,
                peer_connection_options,
                // This shouldn't be called anyway as we aren't reading/writing to disk, so it's
                // ok not to use a shared one.
                BlockingSpawner::new(1),
                connector,
                client_name_and_version,
            )
            .instrument(debug_span!("read_metainfo_from_peer", ?addr))
            .await
            .with_context(|| format!("error reading metainfo from {addr}"));
            drop(token);
            (addr, ret)
        }
    };

    let mut unordered = FuturesUnordered::new();
    // Addresses whose metadata fetch failed, with the earliest retry time.
    let mut failed = HashMap::<SocketAddr, Instant>::new();

    for a in initial_addrs {
        seen.insert(a);
        unordered.push(read_info_guarded(a));
    }

    let mut addrs_completed = false;

    loop {
        if addrs_completed && unordered.is_empty() {
            return ReadMetainfoResult::ChannelClosed { seen };
        }

        tokio::select! {
            done = unordered.next(), if !unordered.is_empty() => {
                match done {
                    Some((_, Ok((info, info_bytes)))) => return ReadMetainfoResult::Found { info, info_bytes, seen, rx: addrs },
                    Some((addr, Err(e))) => {
                        debug!("{:#}", e);
                        failed.insert(addr, Instant::now() + METAINFO_PEER_RETRY_AFTER);
                    },
                    None => unreachable!()
                }
            }

            next_addr = addrs.next(), if !addrs_completed => {
                match next_addr {
                    Some(addr) => {
                        if should_dial(&mut seen, &mut failed, addr, Instant::now()) {
                            unordered.push(read_info_guarded(addr));
                        }
                        continue;
                    },
                    None => {
                        addrs_completed = true;
                    },
                }
            }
        };
    }
}

/// A peer whose metadata fetch failed is retried if the address stream yields
/// it again (DHT, a re-announce, a live peer feed), but not sooner than this.
/// Without it a single transient failure (connection refused, a seeder at its
/// connection limit) removed that peer for the whole resolution, and a magnet
/// with few peers could hang until the caller's timeout.
const METAINFO_PEER_RETRY_AFTER: Duration = Duration::from_secs(2);

/// Whether a newly yielded address should be dialed: never seen before, or
/// seen, failed, and its retry time has passed. In-flight addresses are not
/// dialed twice.
fn should_dial(
    seen: &mut HashSet<SocketAddr>,
    failed: &mut HashMap<SocketAddr, Instant>,
    addr: SocketAddr,
    now: Instant,
) -> bool {
    if seen.insert(addr) {
        return true;
    }
    match failed.get(&addr) {
        Some(retry_at) if now >= *retry_at => {
            failed.remove(&addr);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use dht::{DhtBuilder, Id20};
    use librqbit_core::peer_id::generate_peer_id;

    use super::*;
    use std::{
        str::FromStr,
        sync::{Arc, Once},
    };

    static LOG_INIT: Once = Once::new();

    #[test]
    fn test_should_dial_retries_failed_peers_after_delay() {
        let mut seen = HashSet::new();
        let mut failed = HashMap::new();
        let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
        let t0 = std::time::Instant::now();
        assert!(should_dial(&mut seen, &mut failed, addr, t0));
        // In flight: not dialed twice.
        assert!(!should_dial(&mut seen, &mut failed, addr, t0));
        // Failed: not before the retry time, then exactly once.
        failed.insert(addr, t0 + METAINFO_PEER_RETRY_AFTER);
        assert!(!should_dial(&mut seen, &mut failed, addr, t0));
        let later = t0 + METAINFO_PEER_RETRY_AFTER;
        assert!(should_dial(&mut seen, &mut failed, addr, later));
        assert!(!should_dial(&mut seen, &mut failed, addr, later));
    }

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

        let peer_rx = dht.get_peers(info_hash, None);
        let peer_id = generate_peer_id(b"-xx1234-");
        match read_metainfo_from_peer_receiver(
            peer_id,
            info_hash,
            Vec::new(),
            peer_rx,
            None,
            Arc::new(StreamConnector::new(Default::default()).await.unwrap()),
            crate::client_name_and_version().to_owned(),
        )
        .await
        {
            ReadMetainfoResult::Found { info, .. } => dbg!(info),
            ReadMetainfoResult::ChannelClosed { .. } => todo!("should not have happened"),
        };
    }
}
