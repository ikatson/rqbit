use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{bail, Context};
use buffers::{ByteBuf, ByteBufOwned};
use clone_to_owned::CloneToOwned;
use librqbit_core::{
    hash_id::Id20,
    lengths::{ChunkInfo, ValidPieceIndex},
    peer_id::try_decode_peer_id,
};
use parking_lot::RwLock;
use peer_binary_protocol::{
    extended::{handshake::ExtendedHandshake, ExtendedMessage, PeerIP},
    serialize_piece_preamble, Handshake, Message, MessageOwned, PIECE_MESSAGE_DEFAULT_LEN,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::timeout,
};
use tracing::{debug, trace};

use crate::{read_buf::ReadBuf, spawn_utils::BlockingSpawner, stream_connect::StreamConnector};

pub trait PeerConnectionHandler {
    fn on_connected(&self, _connection_time: Duration) {}
    fn should_send_bitfield(&self) -> bool;
    fn serialize_bitfield_message_to_buf(&self, buf: &mut Vec<u8>) -> anyhow::Result<usize>;
    fn on_handshake<B>(&self, handshake: Handshake<B>) -> anyhow::Result<()>;
    fn on_extended_handshake(
        &self,
        extended_handshake: &ExtendedHandshake<ByteBuf>,
    ) -> anyhow::Result<()>;
    async fn on_received_message(&self, msg: Message<ByteBuf<'_>>) -> anyhow::Result<()>;
    fn should_transmit_have(&self, id: ValidPieceIndex) -> bool;
    fn on_uploaded_bytes(&self, bytes: u32);
    fn read_chunk(&self, chunk: &ChunkInfo, buf: &mut [u8]) -> anyhow::Result<()>;
    fn update_my_extended_handshake(
        &self,
        _handshake: &mut ExtendedHandshake<ByteBuf>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub enum WriterRequest {
    Message(MessageOwned),
    ReadChunkRequest(ChunkInfo),
    Disconnect(anyhow::Result<()>),
}

#[serde_as]
#[derive(Default, Debug, Copy, Clone, Serialize, Deserialize)]
pub struct PeerConnectionOptions {
    #[serde_as(as = "Option<serde_with::DurationSeconds>")]
    pub connect_timeout: Option<Duration>,

    #[serde_as(as = "Option<serde_with::DurationSeconds>")]
    pub read_write_timeout: Option<Duration>,

    #[serde_as(as = "Option<serde_with::DurationSeconds>")]
    pub keep_alive_interval: Option<Duration>,
}

pub(crate) struct PeerConnection<H> {
    handler: H,
    addr: SocketAddr,
    info_hash: Id20,
    peer_id: Id20,
    options: PeerConnectionOptions,
    spawner: BlockingSpawner,
    connector: Arc<StreamConnector>,
}

pub(crate) async fn with_timeout<T, E>(
    timeout_value: Duration,
    fut: impl std::future::Future<Output = Result<T, E>>,
) -> anyhow::Result<T>
where
    E: Into<anyhow::Error>,
{
    match timeout(timeout_value, fut).await {
        Ok(v) => v.map_err(Into::into),
        Err(_) => anyhow::bail!("timeout at {timeout_value:?}"),
    }
}

struct ManagePeerArgs<R, W> {
    handshake_supports_extended: bool,
    read_buf: ReadBuf,
    write_buf: Vec<u8>,
    read: R,
    write: W,
    outgoing_chan: tokio::sync::mpsc::UnboundedReceiver<WriterRequest>,
    have_broadcast: tokio::sync::broadcast::Receiver<ValidPieceIndex>,
}

impl<H: PeerConnectionHandler> PeerConnection<H> {
    pub fn new(
        addr: SocketAddr,
        info_hash: Id20,
        peer_id: Id20,
        handler: H,
        options: Option<PeerConnectionOptions>,
        spawner: BlockingSpawner,
        connector: Arc<StreamConnector>,
    ) -> Self {
        PeerConnection {
            handler,
            addr,
            info_hash,
            peer_id,
            spawner,
            options: options.unwrap_or_default(),
            connector,
        }
    }

    // By the time this is called:
    // read_buf should start with valuable data. The handshake should be removed from it.
    pub async fn manage_peer_incoming(
        &self,
        outgoing_chan: tokio::sync::mpsc::UnboundedReceiver<WriterRequest>,
        read_buf: ReadBuf,
        handshake: Handshake<ByteBufOwned>,
        read: Box<dyn AsyncRead + Unpin + Send + Sync + 'static>,
        mut write: Box<dyn AsyncWrite + Unpin + Send + Sync + 'static>,
        have_broadcast: tokio::sync::broadcast::Receiver<ValidPieceIndex>,
    ) -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt;

        let rwtimeout = self
            .options
            .read_write_timeout
            .unwrap_or_else(|| Duration::from_secs(10));

        if handshake.info_hash != self.info_hash.0 {
            anyhow::bail!("wrong info hash");
        }

        if handshake.peer_id == self.peer_id.0 {
            bail!("looks like we are connecting to ourselves");
        }

        trace!(
            "incoming connection: id={:?}",
            try_decode_peer_id(Id20::new(handshake.peer_id))
        );

        let mut write_buf = Vec::<u8>::with_capacity(PIECE_MESSAGE_DEFAULT_LEN);
        let handshake = Handshake::new(self.info_hash, self.peer_id);
        handshake.serialize(&mut write_buf);
        with_timeout(rwtimeout, write.write_all(&write_buf))
            .await
            .context("error writing handshake")?;
        write_buf.clear();

        let handshake_supports_extended = handshake.supports_extended();

        self.handler.on_handshake(handshake)?;

        self.manage_peer(ManagePeerArgs {
            handshake_supports_extended,
            read_buf,
            write_buf,
            read,
            write,
            outgoing_chan,
            have_broadcast,
        })
        .await
    }

    pub async fn manage_peer_outgoing(
        &self,
        outgoing_chan: tokio::sync::mpsc::UnboundedReceiver<WriterRequest>,
        have_broadcast: tokio::sync::broadcast::Receiver<ValidPieceIndex>,
    ) -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt;
        let rwtimeout = self
            .options
            .read_write_timeout
            .unwrap_or_else(|| Duration::from_secs(10));

        let connect_timeout = self
            .options
            .connect_timeout
            .unwrap_or_else(|| Duration::from_secs(10));

        let now = Instant::now();
        let (mut read, mut write) =
            with_timeout(connect_timeout, self.connector.connect(self.addr))
                .await
                .context("error connecting")?;
        self.handler.on_connected(now.elapsed());

        let mut write_buf = Vec::<u8>::with_capacity(PIECE_MESSAGE_DEFAULT_LEN);
        let handshake = Handshake::new(self.info_hash, self.peer_id);
        handshake.serialize(&mut write_buf);
        with_timeout(rwtimeout, write.write_all(&write_buf))
            .await
            .context("error writing handshake")?;
        write_buf.clear();

        let mut read_buf = ReadBuf::new();
        let h = read_buf
            .read_handshake(&mut read, rwtimeout)
            .await
            .context("error reading handshake")?;
        let handshake_supports_extended = h.supports_extended();
        trace!(
            peer_id=?Id20::new(h.peer_id),
            decoded_id=?try_decode_peer_id(Id20::new(h.peer_id)),
            "connected",
        );
        if h.info_hash != self.info_hash.0 {
            anyhow::bail!("info hash does not match");
        }

        if h.peer_id == self.peer_id.0 {
            bail!("looks like we are connecting to ourselves");
        }

        self.handler.on_handshake(h)?;

        self.manage_peer(ManagePeerArgs {
            handshake_supports_extended,
            read_buf,
            write_buf,
            read,
            write,
            outgoing_chan,
            have_broadcast,
        })
        .await
    }

    async fn manage_peer(
        &self,
        args: ManagePeerArgs<
            impl tokio::io::AsyncRead + Send + Unpin,
            impl tokio::io::AsyncWrite + Send + Unpin,
        >,
    ) -> anyhow::Result<()> {
        let ManagePeerArgs {
            handshake_supports_extended,
            mut read_buf,
            mut write_buf,
            mut read,
            mut write,
            mut outgoing_chan,
            mut have_broadcast,
        } = args;

        use tokio::io::AsyncWriteExt;

        let rwtimeout = self
            .options
            .read_write_timeout
            .unwrap_or_else(|| Duration::from_secs(10));

        let extended_handshake: RwLock<Option<ExtendedHandshake<ByteBufOwned>>> = RwLock::new(None);
        let extended_handshake_ref = &extended_handshake;
        let supports_extended = handshake_supports_extended;

        if supports_extended {
            let mut my_extended = ExtendedHandshake::new();
            my_extended.v = Some(ByteBuf(crate::client_name_and_version().as_bytes()));
            my_extended.yourip = Some(PeerIP(self.addr.ip()));
            self.handler
                .update_my_extended_handshake(&mut my_extended)?;
            let my_extended = Message::Extended(ExtendedMessage::Handshake(my_extended));
            trace!("sending extended handshake: {:?}", &my_extended);
            my_extended
                .serialize(&mut write_buf, &Default::default)
                .unwrap();
            with_timeout(rwtimeout, write.write_all(&write_buf))
                .await
                .context("error writing extended handshake")?;
            write_buf.clear();
        }

        let writer = async move {
            let keep_alive_interval = self
                .options
                .keep_alive_interval
                .unwrap_or_else(|| Duration::from_secs(120));

            if self.handler.should_send_bitfield() {
                let len = self
                    .handler
                    .serialize_bitfield_message_to_buf(&mut write_buf)?;
                with_timeout(rwtimeout, write.write_all(&write_buf[..len]))
                    .await
                    .context("error writing bitfield to peer")?;
                trace!("sent bitfield");
            }

            let len = MessageOwned::Unchoke.serialize(&mut write_buf, &Default::default)?;
            with_timeout(rwtimeout, write.write_all(&write_buf[..len]))
                .await
                .context("error writing unchoke")?;
            trace!("sent unchoke");

            let mut broadcast_closed = false;

            loop {
                let req = loop {
                    break tokio::select! {
                        r = have_broadcast.recv(), if !broadcast_closed => match r {
                            Ok(id) => {
                                if self.handler.should_transmit_have(id) {
                                     WriterRequest::Message(MessageOwned::Have(id.get()))
                                } else {
                                    continue
                                }
                            },
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                broadcast_closed = true;
                                debug!("broadcast channel closed, will not poll it anymore");
                                continue
                            },
                            _ => continue
                        },
                        r = timeout(keep_alive_interval, outgoing_chan.recv()) => match r {
                            Ok(Some(msg)) => msg,
                            Ok(None) => {
                                anyhow::bail!("closing writer, channel closed");
                            }
                            Err(_) => WriterRequest::Message(MessageOwned::KeepAlive),
                        }
                    };
                };

                tokio::task::yield_now().await;

                let mut uploaded_add = None;

                trace!("about to send: {:?}", &req);
                let len = match req {
                    WriterRequest::Message(msg) => msg.serialize(&mut write_buf, &|| {
                        extended_handshake_ref
                            .read()
                            .as_ref()
                            .map(|e| e.peer_extended_messages())
                            .unwrap_or_default()
                    })?,
                    WriterRequest::ReadChunkRequest(chunk) => {
                        #[allow(unused_mut)]
                        let mut skip_reading_for_e2e_tests = false;

                        #[cfg(test)]
                        {
                            use tracing::warn;
                            // This is poor-mans fault injection for running e2e tests.
                            use crate::tests::test_util::TestPeerMetadata;
                            let tpm = TestPeerMetadata::from_peer_id(self.peer_id);
                            use rand::Rng;
                            if rand::thread_rng().gen_bool(tpm.disconnect_probability()) {
                                bail!("disconnecting, to simulate failure in tests");
                            }

                            #[allow(clippy::cast_possible_truncation)]
                            let sleep_ms = (rand::thread_rng().gen::<f64>()
                                * (tpm.max_random_sleep_ms as f64))
                                as u64;
                            tokio::time::sleep(Duration::from_millis(sleep_ms)).await;

                            if rand::thread_rng().gen_bool(tpm.bad_data_probability()) {
                                warn!("will NOT actually read the data to simulate a malicious peer that sends garbage");
                                write_buf.fill(0);
                                skip_reading_for_e2e_tests = true;
                            }
                        }

                        // this whole section is an optimization
                        write_buf.resize(PIECE_MESSAGE_DEFAULT_LEN, 0);
                        let preamble_len = serialize_piece_preamble(&chunk, &mut write_buf);
                        let full_len = preamble_len + chunk.size as usize;
                        write_buf.resize(full_len, 0);
                        if !skip_reading_for_e2e_tests {
                            self.spawner
                                .spawn_block_in_place(|| {
                                    self.handler
                                        .read_chunk(&chunk, &mut write_buf[preamble_len..])
                                })
                                .with_context(|| format!("error reading chunk {chunk:?}"))?;
                        }

                        uploaded_add = Some(chunk.size);
                        full_len
                    }
                    WriterRequest::Disconnect(res) => {
                        trace!("disconnect requested, closing writer");
                        return res;
                    }
                };

                with_timeout(rwtimeout, write.write_all(&write_buf[..len]))
                    .await
                    .context("error writing the message to peer")?;

                if let Some(uploaded_add) = uploaded_add {
                    self.handler.on_uploaded_bytes(uploaded_add)
                }
            }

            // For type inference.
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        };

        let reader = async move {
            loop {
                let message = read_buf
                    .read_message(&mut read, rwtimeout)
                    .await
                    .context("error reading message")?;
                trace!("received: {:?}", &message);

                tokio::task::yield_now().await;

                if let Message::Extended(ExtendedMessage::Handshake(h)) = &message {
                    *extended_handshake_ref.write() = Some(h.clone_to_owned(None));
                    self.handler.on_extended_handshake(h)?;
                } else {
                    self.handler
                        .on_received_message(message)
                        .await
                        .context("error in handler.on_received_message()")?;
                }
            }

            // For type inference.
            #[allow(unreachable_code)]
            Ok::<_, anyhow::Error>(())
        };

        tokio::select! {
            r = reader => {
                if let Err(e) = r.as_ref() {
                    trace!("reader finished with error: {e:#}");
                } else {
                    trace!("reader finished without error");
                }
                r
            }
            r = writer => {
                if let Err(e) = r.as_ref() {
                    trace!("writer finished with error: {e:#}");
                } else {
                    trace!("writer finished without error");
                }
                r
            }
        }
    }
}
