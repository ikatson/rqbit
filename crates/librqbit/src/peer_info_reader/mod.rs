use std::{net::SocketAddr, sync::Arc};

use bencode::from_bytes;
use buffers::{ByteBuf, ByteBufOwned};
use bytes::Bytes;
use librqbit_core::{
    constants::CHUNK_SIZE,
    hash_id::Id20,
    lengths::{last_element_size, ChunkInfo},
    torrent_metainfo::TorrentMetaV1Info,
};
use parking_lot::{Mutex, RwLock};
use peer_binary_protocol::{
    extended::{handshake::ExtendedHandshake, ut_metadata::UtMetadata, ExtendedMessage},
    Handshake, Message,
};
use sha1w::{ISha1, Sha1};
use tokio::sync::mpsc::UnboundedSender;
use tracing::trace;

use crate::{
    peer_connection::{
        PeerConnection, PeerConnectionHandler, PeerConnectionOptions, WriterRequest,
    },
    spawn_utils::BlockingSpawner,
    stream_connect::StreamConnector,
};

pub(crate) async fn read_metainfo_from_peer(
    addr: SocketAddr,
    peer_id: Id20,
    info_hash: Id20,
    peer_connection_options: Option<PeerConnectionOptions>,
    spawner: BlockingSpawner,
    connector: Arc<StreamConnector>,
) -> anyhow::Result<TorrentAndInfoBytes> {
    let (result_tx, result_rx) = tokio::sync::oneshot::channel::<
        anyhow::Result<(TorrentMetaV1Info<ByteBufOwned>, ByteBufOwned)>,
    >();
    let (writer_tx, writer_rx) = tokio::sync::mpsc::unbounded_channel::<WriterRequest>();
    let handler = Handler {
        addr,
        info_hash,
        writer_tx,
        result_tx: Mutex::new(Some(result_tx)),
        locked: RwLock::new(None),
    };
    let connection = PeerConnection::new(
        addr,
        info_hash,
        peer_id,
        handler,
        peer_connection_options,
        spawner,
        connector,
    );

    let result_reader = async move { result_rx.await? };
    let (_, brx) = tokio::sync::broadcast::channel(1);
    let connection_runner = async move { connection.manage_peer_outgoing(writer_rx, brx).await };

    tokio::select! {
        result = result_reader => result,
        whatever = connection_runner => match whatever {
            Ok(_) => anyhow::bail!("connection runner completed first"),
            Err(e) => Err(e)
        }
    }
}

#[derive(Default)]
struct HandlerLocked {
    metadata_size: u32,
    total_pieces: usize,
    buffer: Vec<u8>,
    received_pieces: Vec<bool>,
}

impl HandlerLocked {
    fn new(metadata_size: u32) -> anyhow::Result<Self> {
        if metadata_size > 1024 * 1024 {
            anyhow::bail!("metadata size {} is too big", metadata_size);
        }
        let buffer = vec![0u8; metadata_size as usize];
        let total_pieces: usize = (metadata_size as u64)
            .div_ceil(CHUNK_SIZE as u64)
            .try_into()?;
        let received_pieces = vec![false; total_pieces];
        Ok(Self {
            metadata_size,
            received_pieces,
            buffer,
            total_pieces,
        })
    }
    fn piece_size(&self, index: u32) -> usize {
        if index as usize == self.total_pieces - 1 {
            last_element_size(self.metadata_size as u64, CHUNK_SIZE as u64)
                .try_into()
                .unwrap()
        } else {
            CHUNK_SIZE as usize
        }
    }
    fn record_piece(&mut self, index: u32, data: &[u8], info_hash: Id20) -> anyhow::Result<bool> {
        if index as usize >= self.total_pieces {
            anyhow::bail!("wrong index");
        }
        let offset = (index * CHUNK_SIZE) as usize;
        let size = self.piece_size(index);
        if data.len() != size {
            anyhow::bail!(
                "expected length of piece {} to be {}, but got {}",
                index,
                size,
                data.len()
            );
        }
        if self.received_pieces[index as usize] {
            anyhow::bail!("already received piece {}", index);
        }
        let offset_end = offset + size;
        self.buffer[offset..offset_end].copy_from_slice(data);
        self.received_pieces[index as usize] = true;

        if self.received_pieces.iter().all(|p| *p) {
            // check metadata
            let mut hash = Sha1::new();
            hash.update(&self.buffer);
            if hash.finish() != info_hash.0 {
                anyhow::bail!("info checksum invalid");
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

pub type TorrentAndInfoBytes = (TorrentMetaV1Info<ByteBufOwned>, ByteBufOwned);

struct Handler {
    addr: SocketAddr,
    info_hash: Id20,
    writer_tx: UnboundedSender<WriterRequest>,
    result_tx: Mutex<Option<tokio::sync::oneshot::Sender<anyhow::Result<TorrentAndInfoBytes>>>>,
    locked: RwLock<Option<HandlerLocked>>,
}

impl PeerConnectionHandler for Handler {
    fn should_send_bitfield(&self) -> bool {
        false
    }

    fn serialize_bitfield_message_to_buf(&self, _buf: &mut Vec<u8>) -> anyhow::Result<usize> {
        Ok(0)
    }

    fn on_handshake<B>(&self, handshake: Handshake<B>) -> anyhow::Result<()> {
        if !handshake.supports_extended() {
            anyhow::bail!("this peer does not support extended handshaking, which is a prerequisite to download metadata")
        }
        Ok(())
    }

    async fn on_received_message(&self, msg: Message<ByteBuf<'_>>) -> anyhow::Result<()> {
        trace!("{}: received message: {:?}", self.addr, msg);

        if let Message::Extended(ExtendedMessage::UtMetadata(UtMetadata::Data {
            piece,
            total_size: _,
            data,
        })) = msg
        {
            let piece_ready =
                self.locked
                    .write()
                    .as_mut()
                    .unwrap()
                    .record_piece(piece, &data, self.info_hash)?;
            if piece_ready {
                let buf = Bytes::from(self.locked.write().take().unwrap().buffer);
                let info = from_bytes::<TorrentMetaV1Info<ByteBuf>>(&buf)
                    .map(|i| {
                        use clone_to_owned::CloneToOwned;
                        i.clone_to_owned(Some(&buf))
                    })
                    .map(|i| (i, ByteBufOwned(buf)));

                self.result_tx
                    .lock()
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("oneshot is consumed"))?
                    .send(info)
                    .map_err(|_| {
                        anyhow::anyhow!("torrent info deserialized, but consumer closed")
                    })?;
            }
        }
        Ok(())
    }

    fn on_uploaded_bytes(&self, _bytes: u32) {}

    fn read_chunk(&self, _chunk: &ChunkInfo, _buf: &mut [u8]) -> anyhow::Result<()> {
        anyhow::bail!("the peer is not supposed to be requesting chunks")
    }

    fn on_extended_handshake(
        &self,
        extended_handshake: &ExtendedHandshake<ByteBuf>,
    ) -> anyhow::Result<()> {
        let metadata_size = match extended_handshake.metadata_size {
            Some(metadata_size) => metadata_size,
            None => anyhow::bail!("peer does not have metadata_size"),
        };

        if extended_handshake.ut_metadata().is_none() {
            anyhow::bail!("peer does not support ut_metadata");
        }

        self.writer_tx
            .send(WriterRequest::Message(Message::Unchoke))?;
        self.writer_tx
            .send(WriterRequest::Message(Message::Interested))?;

        let inner = HandlerLocked::new(metadata_size)?;
        let total_pieces = inner.total_pieces;

        self.locked.write().replace(inner);

        for i in 0..total_pieces {
            self.writer_tx
                .send(WriterRequest::Message(Message::Extended(
                    ExtendedMessage::UtMetadata(UtMetadata::Request(i.try_into()?)),
                )))?;
        }
        Ok(())
    }

    fn should_transmit_have(&self, _id: librqbit_core::lengths::ValidPieceIndex) -> bool {
        false
    }
}
