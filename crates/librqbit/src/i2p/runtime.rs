//! The I2P side of a session: one SAM `STREAM` session, kept alive by a supervisor task,
//! the address book, outgoing streams to peers and trackers, and the incoming streams.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, bail};
use futures::{FutureExt, future::BoxFuture};
use librqbit_core::{
    i2p::{I2pDestination, I2pHash},
    spawn_utils::spawn_with_cancel,
};
use parking_lot::Mutex;
use tokio::{
    net::tcp::OwnedWriteHalf,
    sync::{mpsc, watch},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, debug_span, info, warn};
use tracker_comms::{I2pTrackerStream, I2pTransport};

use super::{
    I2pOptions,
    book::{AddressBook, DEFAULT_CAPACITY},
    sam::{SamConn, SamReadHalf, TunnelOptions, new_session_id},
};
use crate::{
    listen::Accept, stream_connect::ConnectionKind, vectored_traits::AsyncReadVectored,
    vectored_traits::AsyncReadVectoredIntoCompat,
};

/// How long `SESSION CREATE` may take: the router builds the tunnels of the session first.
const SESSION_CREATE_TIMEOUT: Duration = Duration::from_secs(180);
/// How long a `NAMING LOOKUP` may take: a `.b32.i2p` name needs a network database lookup.
const NAMING_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a `STREAM CONNECT` may take. Peer connections use the larger of this and their
/// own connect timeout.
pub(crate) const STREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

/// Waits between attempts to (re)create the SAM session.
#[cfg(not(test))]
const BACKOFF: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(60));
#[cfg(test)]
const BACKOFF: (Duration, Duration) = (Duration::from_millis(50), Duration::from_millis(400));

/// How many `STREAM ACCEPT`s are kept pending. SAM 3.1 (the version spoken) refuses a second
/// one on the same session with `ALREADY_ACCEPTING`, so there is one, posted again as soon as
/// a peer arrived; the router queues the streams that arrive meanwhile.
const PENDING_ACCEPTS: usize = 1;

/// Incoming streams waiting for the session's listener.
const INCOMING_QUEUE: usize = 16;

/// Names of trackers (`tracker.i2p`, `….b32.i2p`) resolved, at most this many.
const MAX_NAMES: usize = 1024;

/// A SAM session as long as it lives.
pub(crate) struct SamSession {
    id: String,
    destination: I2pDestination,
    /// Cancelled when the session ended.
    alive: CancellationToken,
}

#[derive(Clone)]
enum SamState {
    /// Nobody needed I2P yet.
    Idle,
    Connecting,
    Up(Arc<SamSession>),
    /// The last attempt failed or the session ended; a new attempt is due after a backoff.
    Down(Arc<str>),
}

type IncomingStream = (SocketAddr, SamReadHalf, OwnedWriteHalf);

/// See the module documentation.
pub(crate) struct I2pRuntime {
    options: I2pOptions,
    book: AddressBook,
    names: Mutex<HashMap<String, I2pDestination>>,
    state: watch::Sender<SamState>,
    /// How many SAM sessions were created so far.
    created: watch::Sender<u64>,
    started: AtomicBool,
    incoming_tx: mpsc::Sender<IncomingStream>,
    cancel: CancellationToken,
    span: tracing::Span,
    user_agent: String,
}

impl std::fmt::Debug for I2pRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("I2pRuntime")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl I2pRuntime {
    /// The runtime, and what the session listens on for incoming streams.
    pub fn new(
        options: I2pOptions,
        cancel: CancellationToken,
        span: tracing::Span,
        user_agent: String,
    ) -> (Arc<Self>, I2pIncoming) {
        let (incoming_tx, incoming_rx) = mpsc::channel(INCOMING_QUEUE);
        let this = Arc::new(Self {
            options,
            book: AddressBook::new(DEFAULT_CAPACITY),
            names: Mutex::new(HashMap::new()),
            state: watch::Sender::new(SamState::Idle),
            created: watch::Sender::new(0),
            started: AtomicBool::new(false),
            incoming_tx,
            cancel,
            span,
            user_agent,
        });
        (
            this,
            I2pIncoming {
                rx: tokio::sync::Mutex::new(incoming_rx),
            },
        )
    }

    pub fn options(&self) -> &I2pOptions {
        &self.options
    }

    /// Changes every time a SAM session is created, to the number of sessions created so far.
    /// Every session has a new transient destination: the I2P trackers must hear about it,
    /// and those that failed while the bridge was unreachable are worth retrying.
    pub fn sessions_created(&self) -> watch::Receiver<u64> {
        self.created.subscribe()
    }

    /// Starts keeping a SAM session, unless done already.
    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        spawn_with_cancel(
            debug_span!(parent: self.span.clone(), "i2p"),
            "i2p",
            self.cancel.clone(),
            self.clone().supervise(),
        );
    }

    /// Creates the SAM session, keeps it while it lives and re-creates it, with a growing
    /// pause after failures.
    async fn supervise(self: Arc<Self>) -> anyhow::Result<()> {
        let mut backoff = BACKOFF.0;
        loop {
            self.state.send_replace(SamState::Connecting);
            let created = tokio::time::timeout(SESSION_CREATE_TIMEOUT, self.create_session())
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("SESSION CREATE timed out")));
            match created {
                Ok((conn, session)) => {
                    backoff = BACKOFF.0;
                    info!(
                        destination = session.destination.hash().b32_name(),
                        "I2P session created"
                    );
                    self.state.send_replace(SamState::Up(session.clone()));
                    self.created.send_modify(|n| *n += 1);
                    for _ in 0..PENDING_ACCEPTS {
                        spawn_with_cancel(
                            debug_span!(parent: self.span.clone(), "i2p_accept"),
                            "i2p_accept",
                            session.alive.clone(),
                            self.clone().accept_streams(session.clone()),
                        );
                    }
                    let why = conn.wait_closed().await;
                    session.alive.cancel();
                    warn!("the I2P session ended: {why}");
                    self.state.send_replace(SamState::Down(
                        format!("the I2P session ended: {why}").into(),
                    ));
                }
                Err(e) => {
                    warn!(sam = %self.options.sam_addr, "can't create the I2P session: {e:#}");
                    self.state.send_replace(SamState::Down(
                        format!(
                            "can't create the I2P session at the SAM bridge {}: {e:#}",
                            self.options.sam_addr
                        )
                        .into(),
                    ));
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF.1);
        }
    }

    async fn create_session(&self) -> anyhow::Result<(SamConn, Arc<SamSession>)> {
        let mut conn = SamConn::open(self.options.sam_addr).await?;
        let id = new_session_id();
        conn.session_create(
            &id,
            &TunnelOptions {
                inbound_quantity: self.options.inbound_quantity,
                outbound_quantity: self.options.outbound_quantity,
                inbound_length: self.options.inbound_length,
                outbound_length: self.options.outbound_length,
            },
        )
        .await?;
        let destination = conn
            .naming_lookup("ME")
            .await
            .context("looking up our own destination")?;
        Ok((
            conn,
            Arc::new(SamSession {
                id,
                destination,
                alive: self.cancel.child_token(),
            }),
        ))
    }

    /// The session; waits while it is being created. Fails while the bridge is unreachable.
    async fn session(self: &Arc<Self>) -> anyhow::Result<Arc<SamSession>> {
        self.start();
        let mut rx = self.state.subscribe();
        loop {
            match &*rx.borrow_and_update() {
                SamState::Up(s) if !s.alive.is_cancelled() => return Ok(s.clone()),
                SamState::Down(why) => bail!("{why}"),
                _ => {}
            }
            rx.changed().await.context("the session is shutting down")?;
        }
    }

    /// Keeps a `STREAM ACCEPT` pending and hands every peer that arrives to the listener.
    async fn accept_streams(self: Arc<Self>, session: Arc<SamSession>) -> anyhow::Result<()> {
        loop {
            let accepted = async {
                let conn = SamConn::open(self.options.sam_addr).await?;
                conn.stream_accept(&session.id).await?.peer().await
            }
            .await;
            match accepted {
                Ok((destination, read, write)) => {
                    let addr = self.book.insert_destination(destination);
                    debug!(?addr, "incoming I2P stream");
                    if self.incoming_tx.send((addr, read, write)).await.is_err() {
                        return Ok(());
                    }
                }
                Err(e) => {
                    // The session ending ends this task; anything else is retried.
                    debug!("STREAM ACCEPT failed: {e:#}");
                    tokio::time::sleep(BACKOFF.0).await;
                }
            }
        }
    }

    /// Resolves a name with `NAMING LOOKUP`, from the cache when it was resolved before.
    async fn lookup(self: &Arc<Self>, name: &str) -> anyhow::Result<I2pDestination> {
        if let Some(d) = self.names.lock().get(name) {
            return Ok(d.clone());
        }
        let destination = tokio::time::timeout(NAMING_TIMEOUT, async {
            SamConn::open(self.options.sam_addr)
                .await?
                .naming_lookup(name)
                .await
        })
        .await
        .map_err(|_| anyhow::anyhow!("NAMING LOOKUP timed out"))?
        .with_context(|| format!("can't resolve {name:?}"))?;
        let mut names = self.names.lock();
        if names.len() >= MAX_NAMES {
            names.clear();
        }
        names.insert(name.to_owned(), destination.clone());
        Ok(destination)
    }

    async fn stream_connect(
        self: &Arc<Self>,
        session: &SamSession,
        destination: &I2pDestination,
    ) -> anyhow::Result<(SamReadHalf, OwnedWriteHalf)> {
        let conn = SamConn::open(self.options.sam_addr).await?;
        Ok(conn.stream_connect(&session.id, destination).await?)
    }

    /// Opens a stream to the peer behind a synthetic address.
    pub async fn connect_peer(
        self: &Arc<Self>,
        addr: SocketAddr,
    ) -> anyhow::Result<(SamReadHalf, OwnedWriteHalf)> {
        let (hash, destination) = self
            .book
            .lookup(&addr)
            .context("an I2P address that is not in the address book")?;
        let session = self.session().await?;
        let destination = match destination {
            Some(d) => d,
            None => {
                let d = self.lookup(&hash.b32_name()).await?;
                self.book.set_destination(addr, d.clone());
                d
            }
        };
        self.stream_connect(&session, &destination).await
    }

    /// The `….b32.i2p` name of the peer behind a synthetic address.
    pub fn b32_name(&self, addr: &SocketAddr) -> Option<String> {
        self.book.lookup(addr).map(|(hash, _)| hash.b32_name())
    }
}

/// The transport of the I2P trackers. The runtime needs an `Arc` of itself, hence the
/// wrapper.
pub(crate) struct TrackerTransport(pub Arc<I2pRuntime>);

impl I2pTransport for TrackerTransport {
    fn connect(&self, host: String) -> BoxFuture<'static, anyhow::Result<I2pTrackerStream>> {
        let rt = self.0.clone();
        async move {
            let session = rt.session().await?;
            let destination = rt.lookup(&host).await?;
            let (read, write) = tokio::time::timeout(
                STREAM_CONNECT_TIMEOUT,
                rt.stream_connect(&session, &destination),
            )
            .await
            .map_err(|_| anyhow::anyhow!("STREAM CONNECT timed out"))??;
            Ok(I2pTrackerStream {
                stream: Box::new(tokio::io::join(read, write)),
                our_destination: session.destination.to_i2p_base64(),
            })
        }
        .boxed()
    }

    fn peer_from_hash(&self, hash: I2pHash) -> SocketAddr {
        self.0.book.insert_hash(hash)
    }

    fn peer_from_destination(&self, destination: I2pDestination) -> SocketAddr {
        self.0.book.insert_destination(destination)
    }

    fn user_agent(&self) -> &str {
        &self.0.user_agent
    }
}

/// The incoming streams of the I2P session, for the session's listener.
pub(crate) struct I2pIncoming {
    rx: tokio::sync::Mutex<mpsc::Receiver<IncomingStream>>,
}

impl Accept for I2pIncoming {
    const KIND: ConnectionKind = ConnectionKind::I2p;

    async fn accept(
        &self,
    ) -> anyhow::Result<(
        SocketAddr,
        (
            impl AsyncReadVectored + Send + 'static,
            impl tokio::io::AsyncWrite + Unpin + Send + 'static,
        ),
    )> {
        match self.rx.lock().await.recv().await {
            Some((addr, read, write)) => Ok((addr, (read.into_vectored_compat(), write))),
            // The runtime is gone, so is the session.
            None => std::future::pending().await,
        }
    }
}
