use std::{
    net::SocketAddr,
    sync::{atomic::AtomicU64, Arc},
    time::{Duration, Instant},
};

use anyhow::{bail, Context};
use bytes::Bytes;
use librqbit::{
    generate_peer_id,
    peer_connection::{self, PeerConnectionHandler, WriterRequest},
    stream_connect::{StreamConnector, StreamConnectorConfig},
    torrent_from_bytes, TorrentMetaV1Owned,
};
use librqbit_core::lengths::{self, Lengths};
use peer_binary_protocol::Message;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tracing::info;

struct Handler {
    received: Arc<AtomicU64>,
    tx: UnboundedSender<WriterRequest>,
    lengths: Lengths,
}

impl PeerConnectionHandler for Handler {
    fn should_send_bitfield(&self) -> bool {
        false
    }

    fn serialize_bitfield_message_to_buf(&self, buf: &mut Vec<u8>) -> anyhow::Result<usize> {
        Ok(buf.len())
    }

    fn on_handshake<B>(&self, handshake: peer_binary_protocol::Handshake<B>) -> anyhow::Result<()> {
        Ok(())
    }

    fn on_extended_handshake(
        &self,
        extended_handshake: &peer_binary_protocol::extended::handshake::ExtendedHandshake<
            buffers::ByteBuf,
        >,
    ) -> anyhow::Result<()> {
        self.tx.send(WriterRequest::Message(Message::Unchoke))?;
        self.tx.send(WriterRequest::Message(Message::Interested))?;

        for piece in self.lengths.iter_piece_infos() {
            for chunk in self.lengths.iter_chunk_infos(piece.piece_index) {
                self.tx
                    .send(WriterRequest::Message(
                        peer_binary_protocol::Message::Request(peer_binary_protocol::Request {
                            index: piece.piece_index.get(),
                            begin: chunk.offset,
                            length: chunk.size,
                        }),
                    ))
                    .unwrap();
            }
        }
        Ok(())
    }

    async fn on_received_message(
        &self,
        msg: peer_binary_protocol::Message<buffers::ByteBuf<'_>>,
    ) -> anyhow::Result<()> {
        if let peer_binary_protocol::Message::Piece(piece) = msg {
            self.received.fetch_add(
                piece.block.len() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        Ok(())
    }

    fn should_transmit_have(&self, id: librqbit_core::lengths::ValidPieceIndex) -> bool {
        false
    }

    fn on_uploaded_bytes(&self, bytes: u32) {}

    fn read_chunk(
        &self,
        chunk: &librqbit_core::lengths::ChunkInfo,
        buf: &mut [u8],
    ) -> anyhow::Result<()> {
        bail!("unsupported")
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();

    let mut args = std::env::args().skip(1);
    let filename = args.next().context("first arg should be filename")?;
    let remote_addr: SocketAddr = args
        .next()
        .context("second arg should be remote addr")?
        .parse()
        .context("can't parse socket addr")?;

    let torrent_bytes = tokio::fs::read(&filename)
        .await
        .context("error reading torrent file")?;

    let torrent: TorrentMetaV1Owned = torrent_from_bytes(&torrent_bytes).context("error")?;
    let lengths = Lengths::from_torrent(&torrent.info)?;

    let connector = Arc::new(
        StreamConnector::new(StreamConnectorConfig::default())
            .await
            .context("error creating connector")?,
    );

    let speed = Arc::new(AtomicU64::new(0));

    let (tx, rx) = unbounded_channel();
    let handler = Handler {
        received: speed.clone(),
        tx: tx.clone(),
        lengths,
    };

    let peer_id = generate_peer_id();

    let conn = peer_connection::PeerConnection::new(
        remote_addr,
        torrent.info_hash,
        peer_id,
        handler,
        Default::default(),
        Default::default(),
        connector,
    );

    let (_btx, brx) = tokio::sync::broadcast::channel(1);

    let manage = conn.manage_peer_outgoing(rx, brx);
    let print_speed = async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        let mut last_bytes = 0u64;
        let mut last_time = Instant::now();
        loop {
            interval.tick().await;
            let bytes = speed.load(std::sync::atomic::Ordering::Relaxed);
            let diff = bytes - last_bytes;
            let elapsed = last_time.elapsed().as_millis() as f64;
            let speed = diff as f64 / elapsed;
            last_bytes = bytes;
            last_time = Instant::now();
            info!("Download speed: {:.2} MB/s)", speed / (1024. * 1024.),);
        }
        #[allow(unused)]
        Ok::<_, anyhow::Error>(())
    };

    tokio::try_join!(manage, print_speed)?;

    Ok(())
}
