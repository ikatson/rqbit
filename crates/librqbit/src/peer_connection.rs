use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    Error, Result, mse::MseMode, session::CheckedIncomingConnection, stream_connect::ConnectionKind,
};
use buffers::{ByteBuf, ByteBufOwned};
use futures::TryFutureExt;
use librqbit_core::{
    hash_id::Id20,
    lengths::{ChunkInfo, ValidPieceIndex},
    peer_id::try_decode_peer_id,
};
use parking_lot::RwLock;
use peer_binary_protocol::{
    Handshake, MAX_MSG_LEN, Message,
    extended::{
        ExtendedMessage, PeerExtendedMessageIds, handshake::ExtendedHandshake,
        ut_metadata::UtMetadata, ut_pex::UtPex,
    },
    serialize_piece_preamble,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use tokio::time::timeout;
use tracing::{Instrument, debug, trace, trace_span};

use crate::{
    read_buf::ReadBuf,
    spawn_utils::BlockingSpawner,
    stream_connect::StreamConnector,
    type_aliases::{BoxAsyncReadVectored, BoxAsyncWrite},
};

pub trait PeerConnectionHandler {
    fn on_connected(&self, _connection_time: Duration) {}
    fn should_send_bitfield(&self) -> bool;
    fn serialize_bitfield_message_to_buf(&self, buf: &mut [u8]) -> anyhow::Result<usize>;
    fn on_handshake(&self, handshake: Handshake, ckind: ConnectionKind) -> anyhow::Result<()>;
    fn on_extended_handshake(
        &self,
        extended_handshake: &ExtendedHandshake<ByteBuf>,
    ) -> anyhow::Result<()>;
    async fn on_received_message(&self, msg: Message<'_>) -> anyhow::Result<()>;
    fn should_transmit_have(&self, id: ValidPieceIndex) -> bool;
    fn on_uploaded_bytes(&self, bytes: u32);
    fn read_chunk(&self, chunk: &ChunkInfo, buf: &mut [u8]) -> anyhow::Result<()>;
    fn update_my_extended_handshake(
        &self,
        _handshake: &mut ExtendedHandshake<ByteBuf>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn client_name_and_version(&self) -> &str {
        crate::client_name_and_version()
    }
}

#[derive(Debug)]
pub enum WriterRequest {
    Message(Message<'static>),
    UtMetadata(UtMetadata<ByteBufOwned>),
    UtPex(UtPex<ByteBufOwned>),
    ReadChunkRequest(ChunkInfo),
    Disconnect(anyhow::Result<()>),
}

#[serde_as]
#[derive(Default, Debug, Copy, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct PeerConnectionOptions {
    #[serde_as(as = "Option<serde_with::DurationSeconds>")]
    pub connect_timeout: Option<Duration>,

    #[serde_as(as = "Option<serde_with::DurationSeconds>")]
    pub read_write_timeout: Option<Duration>,

    #[serde_as(as = "Option<serde_with::DurationSeconds>")]
    pub keep_alive_interval: Option<Duration>,

    /// How MSE (Message Stream Encryption) is applied to this peer connection.
    /// Defaults to [`MseMode::Disabled`].
    pub mse_mode: MseMode,
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

#[cfg(not(feature = "miri"))]
pub(crate) async fn with_timeout<T>(
    name: &'static str,
    timeout_value: Duration,
    fut: impl std::future::Future<Output = Result<T>>,
) -> crate::Result<T> {
    match timeout(timeout_value, fut).await {
        Ok(v) => v,
        Err(_) => Err(Error::Timeout(name)),
    }
}

#[cfg(feature = "miri")]
pub(crate) async fn with_timeout<T>(
    _name: &'static str,
    _timeout_value: Duration,
    fut: impl std::future::Future<Output = Result<T>>,
) -> crate::Result<T> {
    fut.await
}

struct ManagePeerArgs {
    handshake_supports_extended: bool,
    read_buf: ReadBuf,
    write_buf: Box<[u8; MAX_MSG_LEN]>,
    read: BoxAsyncReadVectored,
    write: BoxAsyncWrite,
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
        mut incoming: CheckedIncomingConnection,
        have_broadcast: tokio::sync::broadcast::Receiver<ValidPieceIndex>,
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let rwtimeout = self
            .options
            .read_write_timeout
            .unwrap_or_else(|| Duration::from_secs(10));

        if incoming.handshake.info_hash != self.info_hash {
            return Err(Error::WrongInfoHash);
        }

        if incoming.handshake.peer_id == self.peer_id {
            return Err(Error::ConnectingToOurselves);
        }

        trace!(
            "incoming connection: id={:?}",
            try_decode_peer_id(incoming.handshake.peer_id)
        );

        let mut write_buf = Box::new([0u8; MAX_MSG_LEN]);
        let my_handshake = Handshake::new(self.info_hash, self.peer_id);
        let hlen = my_handshake.serialize_unchecked_len(&mut *write_buf);
        with_timeout(
            "writing handshake",
            rwtimeout,
            incoming
                .writer
                .write_all(&write_buf[..hlen])
                .map_err(Error::WriteHandshake),
        )
        .await?;

        // What the peer supports is in the peer's handshake, not in ours.
        let handshake_supports_extended = incoming.handshake.supports_extended();

        self.handler
            .on_handshake(incoming.handshake, incoming.kind)
            .map_err(Error::Anyhow)?;

        self.manage_peer(ManagePeerArgs {
            handshake_supports_extended,
            read_buf: incoming.read_buf,
            write_buf,
            read: incoming.reader,
            write: incoming.writer,
            outgoing_chan,
            have_broadcast,
        })
        .await
    }

    pub async fn manage_peer_outgoing(
        &self,
        outgoing_chan: tokio::sync::mpsc::UnboundedReceiver<WriterRequest>,
        have_broadcast: tokio::sync::broadcast::Receiver<ValidPieceIndex>,
    ) -> Result<()> {
        let rwtimeout = self
            .options
            .read_write_timeout
            .unwrap_or_else(|| Duration::from_secs(10));

        let connect_timeout = self
            .options
            .connect_timeout
            .unwrap_or_else(|| Duration::from_secs(10));

        let now = Instant::now();
        let conn = self
            .connector
            .connect_with_handshake(
                self.addr,
                &self.info_hash.0,
                &self.peer_id.0,
                connect_timeout,
                rwtimeout,
                self.options.mse_mode,
            )
            .await?;
        let (ckind, mut read, write) = (conn.kind, conn.read, conn.write);

        async move {
            self.handler.on_connected(now.elapsed());
            let write_buf = Box::new([0u8; MAX_MSG_LEN]);

            let mut read_buf = ReadBuf::new();
            let h = read_buf.read_handshake(&mut read, rwtimeout).await?;
            let handshake_supports_extended = h.supports_extended();
            trace!(
                peer_id=?h.peer_id,
                decoded_id=?try_decode_peer_id(h.peer_id),
                "connected",
            );
            if h.info_hash != self.info_hash {
                return Err(Error::WrongInfoHash);
            }

            if h.peer_id == self.peer_id {
                return Err(Error::ConnectingToOurselves);
            }

            self.handler.on_handshake(h, ckind).map_err(Error::Anyhow)?;

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
        .instrument(trace_span!("", kind=%ckind))
        .await
    }

    async fn manage_peer(&self, args: ManagePeerArgs) -> Result<()> {
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

        let extended_handshake: RwLock<Option<PeerExtendedMessageIds>> = RwLock::new(None);
        let extended_handshake_ref = &extended_handshake;
        let supports_extended = handshake_supports_extended;

        if supports_extended {
            let mut my_extended = ExtendedHandshake::new();
            my_extended.v = Some(ByteBuf(self.handler.client_name_and_version().as_bytes()));
            my_extended.yourip = Some(self.addr.ip().into());
            self.handler
                .update_my_extended_handshake(&mut my_extended)
                .map_err(Error::Anyhow)?;
            let my_extended = Message::Extended(ExtendedMessage::Handshake(my_extended));
            trace!("sending extended handshake: {:?}", &my_extended);
            let esz = my_extended.serialize(&mut *write_buf, &Default::default)?;
            with_timeout(
                "writing extended handshake",
                rwtimeout,
                write.write_all(&write_buf[..esz]).map_err(Error::Write),
            )
            .await?;
        }

        let writer = async move {
            let keep_alive_interval = self
                .options
                .keep_alive_interval
                .unwrap_or_else(|| Duration::from_secs(120));

            if self.handler.should_send_bitfield() {
                let len = self
                    .handler
                    .serialize_bitfield_message_to_buf(&mut *write_buf)
                    .map_err(Error::Anyhow)?;
                with_timeout(
                    "writing bitfield",
                    rwtimeout,
                    write.write_all(&write_buf[..len]).map_err(Error::Write),
                )
                .await?;
                trace!("sent bitfield");
            }

            let len = Message::Unchoke.serialize(&mut *write_buf, &Default::default)?;
            with_timeout(
                "writing",
                rwtimeout,
                write.write_all(&write_buf[..len]).map_err(Error::Write),
            )
            .await?;
            trace!("sent unchoke");

            let mut broadcast_closed = false;

            loop {
                let req = loop {
                    break tokio::select! {
                        r = have_broadcast.recv(), if !broadcast_closed => match r {
                            Ok(id) => {
                                if self.handler.should_transmit_have(id) {
                                     WriterRequest::Message(Message::Have(id.get()))
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
                                return Err(Error::TorrentIsNotLive);
                            }
                            Err(_) => WriterRequest::Message(Message::KeepAlive),
                        }
                    };
                };

                tokio::task::yield_now().await;

                let mut uploaded_add = None;

                trace!("about to send: {:?}", &req);
                let ext_msg_ids = &|| {
                    extended_handshake_ref
                        .read()
                        .as_ref()
                        .map(|e| *e)
                        .unwrap_or_default()
                };

                let len = match req {
                    WriterRequest::Message(msg) => msg.serialize(&mut *write_buf, ext_msg_ids)?,
                    WriterRequest::UtMetadata(utm) => {
                        Message::Extended(ExtendedMessage::UtMetadata(utm.as_borrowed()))
                            .serialize(&mut *write_buf, ext_msg_ids)?
                    }
                    WriterRequest::UtPex(ut_pex) => {
                        Message::Extended(ExtendedMessage::UtPex(ut_pex.as_borrowed()))
                            .serialize(&mut *write_buf, ext_msg_ids)?
                    }
                    WriterRequest::ReadChunkRequest(chunk) => {
                        #[allow(unused_mut)]
                        let mut skip_reading_for_e2e_tests = false;

                        #[cfg(test)]
                        {
                            use tracing::warn;
                            // This is poor-mans fault injection for running e2e tests.
                            use crate::tests::test_util::TestPeerMetadata;
                            let tpm = TestPeerMetadata::from_peer_id(self.peer_id);
                            use rand::RngExt;
                            if rand::rng().random_bool(tpm.disconnect_probability()) {
                                return Err(Error::TestDisconnect);
                            }

                            #[allow(clippy::cast_possible_truncation)]
                            let sleep_ms = (rand::rng().random::<f64>()
                                * (tpm.max_random_sleep_ms as f64))
                                as u64;
                            if sleep_ms > 0 {
                                tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                            }

                            if rand::rng().random_bool(tpm.bad_data_probability()) {
                                warn!(
                                    "will NOT actually read the data to simulate a malicious peer that sends garbage"
                                );
                                write_buf.fill(0);
                                skip_reading_for_e2e_tests = true;
                            }
                        }

                        // this whole section is an optimization
                        let preamble_len = serialize_piece_preamble(&chunk, &mut *write_buf);
                        let full_len = preamble_len + chunk.size as usize;
                        if !skip_reading_for_e2e_tests {
                            self.spawner
                                .block_in_place_with_semaphore(|| {
                                    self.handler
                                        .read_chunk(&chunk, &mut write_buf[preamble_len..full_len])
                                })
                                .await
                                .map_err(Error::ReadChunk)?;
                        }

                        uploaded_add = Some(chunk.size);
                        full_len
                    }
                    WriterRequest::Disconnect(res) => {
                        trace!("disconnect requested, closing writer");
                        match res {
                            Ok(()) => return Err(Error::Disconnect),
                            Err(e) => return Err(Error::DisconnectWithSource(e)),
                        }
                    }
                };

                with_timeout(
                    "writing",
                    rwtimeout,
                    write.write_all(&write_buf[..len]).map_err(Error::Write),
                )
                .await?;

                if let Some(uploaded_add) = uploaded_add {
                    self.handler.on_uploaded_bytes(uploaded_add)
                }
            }

            // For type inference.
            #[allow(unreachable_code)]
            Ok::<_, Error>(())
        };

        let reader = async move {
            loop {
                let message = read_buf.read_message(&mut read, rwtimeout).await?;
                trace!("received: {:?}", &message);

                tokio::task::yield_now().await;

                if let Message::Extended(ExtendedMessage::Handshake(h)) = &message {
                    *extended_handshake_ref.write() = Some(h.peer_extended_messages());
                    self.handler
                        .on_extended_handshake(h)
                        .map_err(Error::Anyhow)?;
                } else {
                    self.handler
                        .on_received_message(message)
                        .await
                        .map_err(Error::Anyhow)?;
                }
            }

            // For type inference.
            #[allow(unreachable_code)]
            Ok::<_, Error>(())
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
#[cfg(test)]
mod mse_fallback_tests {
    use super::*;
    use crate::vectored_traits::AsyncReadVectoredIntoCompat;
    use tokio::io::{AsyncReadExt, duplex};

    #[tokio::test]
    async fn fresh_redial_fallback_uses_a_new_stream() -> anyhow::Result<()> {
        let (first_client, mut first_peer) = duplex(4096);
        let (second_client, mut second_peer) = duplex(4096);
        let (first_read, first_write) = tokio::io::split(first_client);
        let (second_read, second_write) = tokio::io::split(second_client);
        let connector = StreamConnector::with_test_connections(vec![
            (
                Box::new(first_read.into_vectored_compat()),
                Box::new(first_write),
            ),
            (
                Box::new(second_read.into_vectored_compat()),
                Box::new(second_write),
            ),
        ]);
        let peer = async move {
            // First connection: read Ya (96 bytes) then drop, so MSE fails.
            let mut first_attempt = [0u8; 96];
            first_peer.read_exact(&mut first_attempt).await?;
            drop(first_peer);
            // Second connection: expect the plaintext BT handshake that
            // connect_with_handshake wrote on the redial.
            let mut plaintext = [0u8; 68];
            second_peer.read_exact(&mut plaintext).await?;
            assert_eq!(&plaintext[..20], b"\x13BitTorrent protocol");
            assert_eq!(&plaintext[28..48], &[0x42; 20]);
            assert_eq!(&plaintext[48..], &[0x11; 20]);
            Ok::<_, std::io::Error>(())
        };
        let client = async {
            let conn = connector
                .connect_with_handshake(
                    "127.0.0.1:1".parse()?,
                    &[0x42; 20],
                    &[0x11; 20],
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    MseMode::Enabled,
                )
                .await?;
            assert!(!conn.mse_applied, "MSE should have failed and fallen back");
            // connect_with_handshake already wrote the plaintext handshake on
            // the redial; drain so the peer's read completes.
            drop(conn.read);
            assert_eq!(connector.remaining_test_connections()?, 0);
            Ok::<_, anyhow::Error>(())
        };
        let (client_result, peer_result) = tokio::join!(client, peer);
        client_result?;
        peer_result?;
        Ok(())
    }

    #[tokio::test]
    async fn disabled_skips_mse_and_uses_single_connection() -> anyhow::Result<()> {
        let (client, mut peer) = duplex(4096);
        let (read, write) = tokio::io::split(client);
        let connector = StreamConnector::with_test_connections(vec![(
            Box::new(read.into_vectored_compat()),
            Box::new(write),
        )]);
        let peer = async move {
            // Disabled MSE: the peer must receive the plaintext handshake
            // directly (68 bytes), never Ya + PadA (96+ bytes first).
            let mut plaintext = [0u8; 68];
            peer.read_exact(&mut plaintext).await?;
            assert_eq!(&plaintext[..20], b"\x13BitTorrent protocol");
            assert_eq!(&plaintext[28..48], &[0x42; 20]);
            assert_eq!(&plaintext[48..], &[0x11; 20]);
            Ok::<_, std::io::Error>(())
        };
        let client = async {
            let conn = connector
                .connect_with_handshake(
                    "127.0.0.1:1".parse()?,
                    &[0x42; 20],
                    &[0x11; 20],
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    MseMode::Disabled,
                )
                .await?;
            assert!(!conn.mse_applied, "MSE must not be attempted in Disabled mode");
            // connect_with_handshake wrote the plaintext handshake already.
            drop(conn.read);
            // Only one connection consumed: mse::outgoing was never invoked.
            assert_eq!(connector.remaining_test_connections()?, 0);
            Ok::<_, anyhow::Error>(())
        };
        let (client_result, peer_result) = tokio::join!(client, peer);
        client_result?;
        peer_result?;
        Ok(())
    }

    #[tokio::test]
    async fn forced_mse_failure_returns_error_without_redial() -> anyhow::Result<()> {
        let (client, mut peer) = duplex(4096);
        let (read, write) = tokio::io::split(client);
        let connector = StreamConnector::with_test_connections(vec![(
            Box::new(read.into_vectored_compat()),
            Box::new(write),
        )]);
        let peer = async move {
            // Read Ya (96 bytes) then drop, so MSE fails.
            let mut ya = [0u8; 96];
            peer.read_exact(&mut ya).await?;
            drop(peer);
            Ok::<_, std::io::Error>(())
        };
        let client = async {
            let result = connector
                .connect_with_handshake(
                    "127.0.0.1:1".parse()?,
                    &[0x42; 20],
                    &[0x11; 20],
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    MseMode::Forced,
                )
                .await;
            assert!(
                matches!(result, Err(Error::MseForced)),
                "Forced mode must error on MSE failure"
            );
            // No redial in Forced mode: exactly one connection was consumed.
            assert_eq!(connector.remaining_test_connections()?, 0);
            Ok::<_, anyhow::Error>(())
        };
        let (client_result, peer_result) = tokio::join!(client, peer);
        client_result?;
        peer_result?;
        Ok(())
    }
}
