use std::{collections::VecDeque, path::PathBuf, sync::Arc};

use bytes::Bytes;
use librqbit_core::lengths::ValidPieceIndex;
use tokio::io::{AsyncRead, AsyncSeek};

use crate::torrent_state::peers::PeerStates;

struct StreamedTorrent {
    peers: PeerStates,
    // lengths
    // info
}

impl StreamedTorrent {
    // Caching can be done later
    async fn get_piece(&self, id: ValidPieceIndex) -> Bytes {
        // check cache
        // fetch all chunks
        // validate checksum
        todo!()
    }
}

struct StreamedFile {
    // for debugging
    filename: PathBuf,
}

struct SingleReadStream {
    torrent: Arc<StreamedTorrent>,
    buffer: VecDeque<u8>,
}

impl AsyncRead for SingleReadStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        todo!()
    }
}

impl AsyncSeek for SingleReadStream {
    fn start_seek(
        self: std::pin::Pin<&mut Self>,
        position: std::io::SeekFrom,
    ) -> std::io::Result<()> {
        todo!()
    }

    fn poll_complete(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<u64>> {
        todo!()
    }
}
