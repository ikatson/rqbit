use std::{
    net::SocketAddr,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use anyhow::Context;
use log::{debug, info, trace, warn};
use tokio::time::timeout;

use crate::{
    buffers::{ByteBuf, ByteString},
    chunk_tracker::ChunkMarkingResult,
    clone_to_owned::CloneToOwned,
    lengths::ChunkInfo,
    peer_binary_protocol::{
        serialize_piece_preamble, Handshake, Message, MessageBorrowed, MessageDeserializeError,
        MessageOwned, Piece, Request, PIECE_MESSAGE_DEFAULT_LEN,
    },
    peer_id::try_decode_peer_id,
    spawn_utils::{spawn, spawn_block_in_place},
    torrent_state::{InflightRequest, TorrentState},
    type_aliases::PeerHandle,
};

#[derive(Debug)]
pub enum WriterRequest {
    Message(MessageOwned),
    ReadChunkRequest(ChunkInfo),
}

#[derive(Clone)]
pub struct PeerConnection {
    state: Arc<TorrentState>,
}

impl PeerConnection {
    pub fn new(state: Arc<TorrentState>) -> Self {
        PeerConnection { state }
    }
    pub fn into_state(self) -> Arc<TorrentState> {
        self.state
    }
    pub async fn manage_peer(
        &self,
        addr: SocketAddr,
        handle: PeerHandle,
        // outgoing_chan_tx: tokio::sync::mpsc::Sender<MessageOwned>,
        mut outgoing_chan: tokio::sync::mpsc::Receiver<WriterRequest>,
    ) -> anyhow::Result<()> {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;
        let mut conn = tokio::net::TcpStream::connect(addr)
            .await
            .context("error connecting")?;
        let handshake = Handshake::new(self.state.info_hash, self.state.peer_id);
        conn.write_all(&handshake.serialize())
            .await
            .context("error writing handshake")?;
        let mut read_buf = vec![0u8; PIECE_MESSAGE_DEFAULT_LEN * 2];
        let read_bytes = conn
            .read(&mut read_buf)
            .await
            .context("error reading handshake")?;
        if read_bytes == 0 {
            anyhow::bail!("bad handshake");
        }
        let (h, hlen) = Handshake::deserialize(&read_buf[..read_bytes])
            .map_err(|e| anyhow::anyhow!("error deserializing handshake: {:?}", e))?;

        let mut read_so_far = 0usize;
        debug!(
            "connected peer {}: {:?}",
            addr,
            try_decode_peer_id(h.peer_id)
        );
        if h.info_hash != self.state.info_hash {
            anyhow::bail!("info hash does not match");
        }

        self.state.set_peer_live(handle, h);

        if read_bytes > hlen {
            read_buf.copy_within(hlen..read_bytes, 0);
            read_so_far = read_bytes - hlen;
        }

        let (mut read_half, mut write_half) = tokio::io::split(conn);

        let this = self.clone();
        let writer = async move {
            let mut buf = Vec::<u8>::with_capacity(PIECE_MESSAGE_DEFAULT_LEN);
            let keep_alive_interval = Duration::from_secs(120);

            if this.state.stats.have.load(Ordering::Relaxed) > 0 {
                let len = {
                    let g = this.state.locked.read();
                    let msg = Message::Bitfield(ByteBuf(g.chunks.get_have_pieces().as_raw_slice()));
                    let len = msg.serialize(&mut buf);
                    debug!("sending to {}: {:?}, length={}", handle, &msg, len);
                    len
                };

                write_half
                    .write_all(&buf[..len])
                    .await
                    .context("error writing bitfield to peer")?;
                debug!("sent bitfield to {}", handle);
            }

            loop {
                let req = match timeout(keep_alive_interval, outgoing_chan.recv()).await {
                    Ok(Some(msg)) => msg,
                    Ok(None) => {
                        anyhow::bail!("closing writer, channel closed")
                    }
                    Err(_) => WriterRequest::Message(MessageOwned::KeepAlive),
                };

                let mut uploaded_add = None;

                let len = match &req {
                    WriterRequest::Message(msg) => msg.serialize(&mut buf),
                    WriterRequest::ReadChunkRequest(chunk) => {
                        // this whole section is an optimization
                        buf.resize(PIECE_MESSAGE_DEFAULT_LEN, 0);
                        let preamble_len = serialize_piece_preamble(&chunk, &mut buf);
                        let full_len = preamble_len + chunk.size as usize;
                        buf.resize(full_len, 0);
                        spawn_block_in_place(|| {
                            this.state.file_ops().read_chunk(
                                handle,
                                &chunk,
                                &mut buf[preamble_len..],
                            )
                        })
                        .with_context(|| format!("error reading chunk {:?}", chunk))?;
                        uploaded_add = Some(chunk.size);
                        full_len
                    }
                };

                debug!("sending to {}: {:?}, length={}", handle, &req, len);

                write_half
                    .write_all(&buf[..len])
                    .await
                    .context("error writing the message to peer")?;

                if let Some(uploaded_add) = uploaded_add {
                    this.state
                        .stats
                        .uploaded
                        .fetch_add(uploaded_add as u64, Ordering::Relaxed);
                }
            }

            // For type inference.
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        };

        let reader = async move {
            loop {
                let (message, size) = loop {
                    match MessageBorrowed::deserialize(&read_buf[..read_so_far]) {
                        Ok((msg, size)) => {
                            break (msg, size);
                        }
                        Err(MessageDeserializeError::NotEnoughData(d, _)) => {
                            if read_buf.len() < read_so_far + d {
                                read_buf.reserve(d);
                                read_buf.resize(read_buf.capacity(), 0);
                            }

                            let size = read_half
                                .read(&mut read_buf[read_so_far..])
                                .await
                                .context("error reading from peer")?;
                            if size == 0 {
                                anyhow::bail!(
                                    "disconnected while reading, read so far: {}",
                                    read_so_far
                                )
                            }
                            read_so_far += size;
                        }
                        Err(e) => return Err(e.into()),
                    }
                };

                trace!("received from {}: {:?}", handle, &message);

                match message {
                    Message::Request(request) => {
                        self.on_download_request(handle, request)
                            .await
                            .with_context(|| {
                                format!("error handling download request from {}", handle)
                            })?;
                    }
                    Message::Bitfield(b) => self.on_bitfield(handle, b.clone_to_owned()).await?,
                    Message::Choke => self.on_i_am_choked(handle),
                    Message::Unchoke => self.on_i_am_unchoked(handle),
                    Message::Interested => self.on_peer_interested(handle),
                    Message::Piece(piece) => {
                        self.on_received_piece(handle, piece)
                            .context("error in on_received_piece()")?;
                    }
                    Message::KeepAlive => {
                        debug!("keepalive received from {}", handle);
                    }
                    Message::Have(h) => self.on_have(handle, h),
                    Message::NotInterested => {
                        info!("received \"not interested\", but we don't care yet")
                    }
                }

                if read_so_far > size {
                    read_buf.copy_within(size..read_so_far, 0);
                }
                read_so_far -= size;
            }

            // For type inference.
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        };

        let r = tokio::select! {
            r = reader => {r}
            r = writer => {r}
        };
        debug!("{}: either reader or writer are done, exiting", handle);
        r
    }

    async fn on_download_request(
        &self,
        peer_handle: PeerHandle,
        request: Request,
    ) -> anyhow::Result<()> {
        let piece_index = match self.state.lengths.validate_piece_index(request.index) {
            Some(p) => p,
            None => {
                anyhow::bail!(
                    "{}: received {:?}, but it is not a valid chunk request (piece index is invalid). Ignoring.",
                    peer_handle, request
                );
            }
        };
        let chunk_info = match self.state.lengths.chunk_info_from_received_data(
            piece_index,
            request.begin,
            request.length,
        ) {
            Some(d) => d,
            None => {
                anyhow::bail!(
                    "{}: received {:?}, but it is not a valid chunk request (chunk data is invalid). Ignoring.",
                    peer_handle, request
                );
            }
        };

        let tx = self
            .state
            .locked
            .read()
            .peers
            .clone_tx(peer_handle)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "peer {} died, dropping chunk that it requested",
                    peer_handle
                )
            })?;

        // TODO: this is not super efficient as it does copying multiple times.
        // Theoretically, this could be done in the sending code, so that it reads straight into
        // the send buffer.
        let request = WriterRequest::ReadChunkRequest(chunk_info);
        debug!("sending to {}: {:?}", peer_handle, &request);
        Ok::<_, anyhow::Error>(tx.send(request).await?)
    }

    fn on_have(&self, handle: PeerHandle, have: u32) {
        if let Some(bitfield) = self
            .state
            .locked
            .write()
            .peers
            .get_live_mut(handle)
            .and_then(|l| l.bitfield.as_mut())
        {
            debug!("{}: updated bitfield with have={}", handle, have);
            bitfield.set(have as usize, true)
        }
    }

    async fn on_bitfield(&self, handle: PeerHandle, bitfield: ByteString) -> anyhow::Result<()> {
        if bitfield.len() != self.state.lengths.piece_bitfield_bytes() as usize {
            anyhow::bail!(
                "dropping {} as its bitfield has unexpected size. Got {}, expected {}",
                handle,
                bitfield.len(),
                self.state.lengths.piece_bitfield_bytes(),
            );
        }
        self.state
            .locked
            .write()
            .peers
            .update_bitfield_from_vec(handle, bitfield.0);

        if !self.state.am_i_interested_in_peer(handle) {
            let tx = self
                .state
                .locked
                .read()
                .peers
                .clone_tx(handle)
                .ok_or_else(|| anyhow::anyhow!("peer closed"))?;
            tx.send(WriterRequest::Message(MessageOwned::Unchoke))
                .await
                .context("peer dropped")?;
            tx.send(WriterRequest::Message(MessageOwned::NotInterested))
                .await
                .context("peer dropped")?;
            return Ok(());
        }

        // Additional spawn per peer, not good.
        spawn(
            format!("peer_chunk_requester({})", handle),
            self.clone().task_peer_chunk_requester(handle),
        );
        Ok(())
    }

    async fn task_peer_chunk_requester(self, handle: PeerHandle) -> anyhow::Result<()> {
        let tx = match self.state.locked.read().peers.clone_tx(handle) {
            Some(tx) => tx,
            None => return Ok(()),
        };
        tx.send(WriterRequest::Message(MessageOwned::Unchoke))
            .await
            .context("peer dropped")?;
        tx.send(WriterRequest::Message(MessageOwned::Interested))
            .await
            .context("peer dropped")?;

        self.requester(handle).await?;
        Ok::<_, anyhow::Error>(())
    }

    fn on_i_am_choked(&self, handle: PeerHandle) {
        warn!("we are choked by {}", handle);
        self.state
            .locked
            .write()
            .peers
            .mark_i_am_choked(handle, true);
    }

    fn on_peer_interested(&self, handle: PeerHandle) {
        debug!("peer {} is interested", handle);
        self.state
            .locked
            .write()
            .peers
            .mark_peer_interested(handle, true);
    }

    async fn requester(self, handle: PeerHandle) -> anyhow::Result<()> {
        let notify = match self.state.locked.read().peers.get_live(handle) {
            Some(l) => l.have_notify.clone(),
            None => return Ok(()),
        };

        // TODO: this might dangle, same below.
        #[allow(unused_must_use)]
        {
            timeout(Duration::from_secs(60), notify.notified()).await;
        }

        loop {
            match self.state.am_i_choked(handle) {
                Some(true) => {
                    warn!("we are choked by {}, can't reserve next piece", handle);
                    #[allow(unused_must_use)]
                    {
                        timeout(Duration::from_secs(60), notify.notified()).await;
                    }
                    continue;
                }
                Some(false) => {}
                None => return Ok(()),
            }

            let next = match self.state.reserve_next_needed_piece(handle) {
                Some(next) => next,
                None => {
                    if self.state.get_left_to_download() == 0 {
                        debug!("{}: nothing left to download, closing requester", handle);
                        return Ok(());
                    }

                    if let Some(piece) = self.state.try_steal_piece(handle) {
                        debug!("{}: stole a piece {}", handle, piece);
                        piece
                    } else {
                        debug!("no pieces to request from {}", handle);
                        #[allow(unused_must_use)]
                        {
                            timeout(Duration::from_secs(60), notify.notified()).await;
                        }
                        continue;
                    }
                }
            };
            let tx = match self.state.locked.read().peers.clone_tx(handle) {
                Some(tx) => tx,
                None => return Ok(()),
            };
            let sem = match self.state.locked.read().peers.get_live(handle) {
                Some(live) => live.requests_sem.clone(),
                None => return Ok(()),
            };
            for chunk in self.state.lengths.iter_chunk_infos(next) {
                if self.state.locked.read().chunks.is_chunk_downloaded(&chunk) {
                    continue;
                }
                if !self
                    .state
                    .locked
                    .write()
                    .peers
                    .try_get_live_mut(handle)?
                    .inflight_requests
                    .insert(InflightRequest::from(&chunk))
                {
                    warn!(
                        "{}: probably a bug, we already requested {:?}",
                        handle, chunk
                    );
                    continue;
                }

                let request = Request {
                    index: next.get(),
                    begin: chunk.offset,
                    length: chunk.size,
                };
                sem.acquire().await?.forget();

                tx.send(WriterRequest::Message(MessageOwned::Request(request)))
                    .await
                    .context("peer dropped")?;
            }
        }
    }

    fn on_i_am_unchoked(&self, handle: PeerHandle) {
        debug!("we are unchoked by {}", handle);
        let mut g = self.state.locked.write();
        let live = match g.peers.get_live_mut(handle) {
            Some(live) => live,
            None => return,
        };
        live.i_am_choked = false;
        live.have_notify.notify_waiters();
        live.requests_sem.add_permits(16);
    }

    fn on_received_piece(&self, handle: PeerHandle, piece: Piece<ByteBuf>) -> anyhow::Result<()> {
        let chunk_info = match self.state.lengths.chunk_info_from_received_piece(&piece) {
            Some(i) => i,
            None => {
                anyhow::bail!(
                    "peer {} sent us a piece that is invalid {:?}",
                    handle,
                    &piece,
                );
            }
        };

        let mut g = self.state.locked.write();
        let h = g.peers.try_get_live_mut(handle)?;
        h.requests_sem.add_permits(1);

        self.state
            .stats
            .fetched_bytes
            .fetch_add(piece.block.len() as u64, Ordering::Relaxed);

        if !h
            .inflight_requests
            .remove(&InflightRequest::from(&chunk_info))
        {
            anyhow::bail!(
                "peer {} sent us a piece that we did not ask it for. Requested pieces: {:?}. Got: {:?}", handle, &h.inflight_requests, &piece,
            );
        }

        let should_checksum = match g.chunks.mark_chunk_downloaded(&piece) {
            Some(ChunkMarkingResult::Completed) => {
                debug!(
                    "piece={} done by {}, will write and checksum",
                    piece.index, handle
                );
                // This will prevent others from stealing it.
                g.peers.remove_inflight_piece(chunk_info.piece_index);
                true
            }
            Some(ChunkMarkingResult::PreviouslyCompleted) => {
                // TODO: we might need to send cancellations here.
                debug!(
                    "piece={} was done by someone else {}, ignoring",
                    piece.index, handle
                );
                return Ok(());
            }
            Some(ChunkMarkingResult::NotCompleted) => false,
            None => {
                anyhow::bail!(
                    "bogus data received from {}: {:?}, cannot map this to a chunk, dropping peer",
                    handle,
                    piece
                );
            }
        };

        // to prevent deadlocks.
        drop(g);

        spawn_block_in_place(move || {
            let index = piece.index;

            // TODO: in theory we should unmark the piece as downloaded here. But if there was a disk error, what
            // should we really do? If we unmark it, it will get requested forever...
            //
            // So let's just unwrap and abort.
            self.state
                .file_ops()
                .write_chunk(handle, &piece, &chunk_info)
                .expect("expected to be able to write to disk");

            if !should_checksum {
                return Ok(());
            }

            match self
                .state
                .file_ops()
                .check_piece(handle, chunk_info.piece_index, &chunk_info)
                .with_context(|| format!("error checking piece={}", index))?
            {
                true => {
                    let piece_len = self.state.lengths.piece_length(chunk_info.piece_index) as u64;
                    self.state
                        .stats
                        .downloaded_and_checked
                        .fetch_add(piece_len, Ordering::Relaxed);
                    self.state
                        .stats
                        .have
                        .fetch_add(piece_len, Ordering::Relaxed);
                    self.state
                        .locked
                        .write()
                        .chunks
                        .mark_piece_downloaded(chunk_info.piece_index);

                    debug!(
                        "piece={} successfully downloaded and verified from {}",
                        index, handle
                    );

                    self.state.maybe_transmit_haves(chunk_info.piece_index);
                }
                false => {
                    warn!(
                        "checksum for piece={} did not validate, came from {}",
                        index, handle
                    );
                    self.state
                        .locked
                        .write()
                        .chunks
                        .mark_piece_broken(chunk_info.piece_index);
                }
            };
            Ok::<_, anyhow::Error>(())
        })
        .with_context(|| format!("error processing received chunk {:?}", chunk_info))?;
        Ok(())
    }
}
