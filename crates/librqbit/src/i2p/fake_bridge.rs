//! An in-process fake SAM v3 bridge, for tests: there is no I2P router on a test machine.
//!
//! It speaks the bridge side of SAM 3.0 to 3.3 the way the I2P router does, but strictly:
//! everything a client does that the protocol does not allow (a command before `HELLO`, an
//! unknown command or key, a duplicate key, a stream command on the session socket, bytes sent
//! on an accepting socket before a peer arrived, invalid tunnel options...) is recorded as a
//! *violation*; tests assert there are none.
//!
//! "Destinations" are random byte strings with the structure of an Ed25519 destination. A
//! `STREAM CONNECT` to the destination of another session is handed to a pending
//! `STREAM ACCEPT` (or a `STREAM FORWARD`) of that session, and the two TCP connections are
//! spliced. Tests can also host *services*: destinations whose streams are spliced to a plain
//! TCP address on loopback (a fake HTTP tracker reachable as `tracker.i2p`, a wire-level peer).
//!
//! Every command line is recorded. The bridge can be taken down (every connection is closed,
//! every session is lost, new connections are closed right away) and brought up again.

use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use librqbit_core::i2p::{I2pDestination, I2pHash, i2p_base64_encode};
use parking_lot::Mutex;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{Notify, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::sam::{Reply, SamError, command, read_line};

type Conn = BufReader<TcpStream>;

/// How long a `STREAM CONNECT` to a session waits for that session to accept.
const ACCEPT_WAIT: Duration = Duration::from_secs(5);

/// A SAM version, `(major, minor)`.
type Version = (u8, u8);

fn parse_version(v: &str) -> Option<Version> {
    let (major, minor) = v.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// A random destination with the structure of an Ed25519 one: 384 bytes of keys and a key
/// certificate (type 5, 4 bytes: signing type 7, crypto type 0).
pub(crate) fn random_destination() -> I2pDestination {
    let mut bytes = Vec::with_capacity(391);
    while bytes.len() < 384 {
        bytes.extend_from_slice(&rand::random::<[u8; 32]>());
    }
    bytes.extend_from_slice(&[5, 0, 4, 0, 7, 0, 0]);
    match I2pDestination::from_bytes(Bytes::from(bytes)) {
        Ok(d) => d,
        Err(e) => unreachable!("a well-formed destination is refused: {e:#}"),
    }
}

/// A session as the bridge sees it.
#[derive(Debug, Clone)]
pub(crate) struct SessionInfo {
    pub id: String,
    pub destination: I2pDestination,
    /// Every option of `SESSION CREATE` besides `STYLE`, `ID` and `DESTINATION`, in order.
    pub options: Vec<(String, String)>,
}

struct BridgeSession {
    info: SessionInfo,
    /// The connection that created it: the session lives as long as it does.
    conn: u64,
    accepts: VecDeque<oneshot::Sender<Incoming>>,
    forwards: Vec<Forward>,
    /// Cancelled when the session ends: its streams end with it.
    alive: CancellationToken,
}

#[derive(Clone)]
struct Forward {
    conn: u64,
    target: SocketAddr,
    silent: bool,
}

/// A connecting stream handed to an accepting one.
struct Incoming {
    from: I2pDestination,
    conn: Conn,
    silent: bool,
    alive: CancellationToken,
}

struct Service {
    dest: I2pDestination,
    target: SocketAddr,
    connections: usize,
}

#[derive(Default)]
struct State {
    commands: Vec<String>,
    violations: Vec<String>,
    connections: usize,
    refused: usize,
    sessions: HashMap<String, BridgeSession>,
    /// Every session that ever existed, in order of creation.
    history: Vec<SessionInfo>,
    services: HashMap<I2pHash, Service>,
    names: HashMap<String, I2pDestination>,
}

struct Shared {
    state: Mutex<State>,
    accept_posted: Notify,
    /// Cancelled when the bridge goes down: every connection ends.
    epoch: Mutex<CancellationToken>,
    down: AtomicBool,
    next_conn: AtomicU64,
    versions: Mutex<(Version, Version)>,
}

impl Shared {
    fn violation(&self, conn: u64, what: String) {
        tracing::warn!(conn, "SAM protocol violation: {what}");
        self.state.lock().violations.push(what);
    }

    fn end_sessions_of(&self, conn: u64) {
        let mut g = self.state.lock();
        g.sessions.retain(|_, s| {
            if s.conn == conn {
                s.alive.cancel();
                false
            } else {
                true
            }
        });
        for s in g.sessions.values_mut() {
            s.forwards.retain(|f| f.conn != conn);
        }
    }

    fn destination_by_hash(&self, hash: &I2pHash) -> Option<I2pDestination> {
        let g = self.state.lock();
        if let Some(s) = g.services.get(hash) {
            return Some(s.dest.clone());
        }
        g.sessions
            .values()
            .map(|s| &s.info.destination)
            .find(|d| d.hash() == *hash)
            .cloned()
    }
}

/// See the module documentation.
pub(crate) struct FakeSamBridge {
    addr: SocketAddr,
    shared: Arc<Shared>,
    task: JoinHandle<()>,
}

impl Drop for FakeSamBridge {
    fn drop(&mut self) {
        self.task.abort();
        self.shared.epoch.lock().cancel();
    }
}

impl FakeSamBridge {
    /// A bridge on a free loopback port, up, speaking SAM 3.0 to 3.3.
    pub async fn start() -> anyhow::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            accept_posted: Notify::new(),
            epoch: Mutex::new(CancellationToken::new()),
            down: AtomicBool::new(false),
            next_conn: AtomicU64::new(0),
            versions: Mutex::new(((3, 0), (3, 3))),
        });
        let s = shared.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                if s.down.load(Ordering::SeqCst) {
                    s.state.lock().refused += 1;
                    drop(stream);
                    continue;
                }
                s.state.lock().connections += 1;
                let id = s.next_conn.fetch_add(1, Ordering::Relaxed);
                let epoch = s.epoch.lock().clone();
                let s = s.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = epoch.cancelled() => {}
                        r = handle(s.clone(), id, BufReader::new(stream)) => {
                            if let Err(e) = r {
                                debug!(id, "fake SAM connection ended: {e:#}");
                            }
                        }
                    }
                    s.end_sessions_of(id);
                });
            }
        });
        Ok(Self { addr, shared, task })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Every command line received, in order.
    pub fn commands(&self) -> Vec<String> {
        self.shared.state.lock().commands.clone()
    }

    /// Every protocol violation seen.
    pub fn violations(&self) -> Vec<String> {
        self.shared.state.lock().violations.clone()
    }

    /// Connections accepted while up.
    pub fn connections(&self) -> usize {
        self.shared.state.lock().connections
    }

    /// Connections closed right away because the bridge was down.
    pub fn refused(&self) -> usize {
        self.shared.state.lock().refused
    }

    /// The live sessions.
    pub fn sessions(&self) -> Vec<SessionInfo> {
        let g = self.shared.state.lock();
        g.sessions.values().map(|s| s.info.clone()).collect()
    }

    /// Every session ever created, in order.
    pub fn session_history(&self) -> Vec<SessionInfo> {
        self.shared.state.lock().history.clone()
    }

    /// Only these versions are spoken from now on.
    pub fn set_versions(&self, min: &str, max: &str) {
        let (Some(min), Some(max)) = (parse_version(min), parse_version(max)) else {
            panic!("bad versions {min:?} {max:?}");
        };
        *self.shared.versions.lock() = (min, max);
    }

    /// Closes every connection, loses every session and refuses new connections.
    pub fn go_down(&self) {
        self.shared.down.store(true, Ordering::SeqCst);
        let mut epoch = self.shared.epoch.lock();
        epoch.cancel();
        *epoch = CancellationToken::new();
        let mut g = self.shared.state.lock();
        for (_, s) in g.sessions.drain() {
            s.alive.cancel();
        }
    }

    pub fn go_up(&self) {
        self.shared.down.store(false, Ordering::SeqCst);
    }

    /// Hosts a service: streams to the returned destination are spliced to `target`. With a
    /// `name`, `NAMING LOOKUP` finds it under that name.
    pub fn add_service(&self, name: Option<&str>, target: SocketAddr) -> I2pDestination {
        let dest = random_destination();
        let mut g = self.shared.state.lock();
        g.services.insert(
            dest.hash(),
            Service {
                dest: dest.clone(),
                target,
                connections: 0,
            },
        );
        if let Some(name) = name {
            g.names.insert(name.to_owned(), dest.clone());
        }
        dest
    }

    /// How many streams were opened to a service.
    pub fn service_connections(&self, dest: &I2pDestination) -> usize {
        let g = self.shared.state.lock();
        g.services
            .get(&dest.hash())
            .map(|s| s.connections)
            .unwrap_or(0)
    }
}

async fn reply(c: &mut Conn, words: &str, pairs: &[(&str, &str)]) -> anyhow::Result<()> {
    let line = command(words, pairs)?;
    c.get_mut().write_all(line.as_bytes()).await?;
    Ok(())
}

/// Checks the keys of a command: only `allowed` ones, plus dotted (I2CP) options when
/// `dotted` is set. `Reply::parse` refuses duplicates already.
fn check_keys(cmd: &Reply, allowed: &[&str], dotted: bool) -> Result<(), String> {
    for (k, _) in cmd.pairs() {
        if !(allowed.contains(&k.as_str()) || (dotted && k.contains('.'))) {
            return Err(format!("unknown key {k:?} in {} {}", cmd.topic, cmd.kind));
        }
    }
    Ok(())
}

fn check_silent(cmd: &Reply) -> Result<bool, String> {
    match cmd.get("SILENT") {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(other) => Err(format!("SILENT={other:?}")),
    }
}

async fn handle(shared: Arc<Shared>, id: u64, mut c: Conn) -> anyhow::Result<()> {
    let mut hello = false;
    let mut session_here: Option<String> = None;
    loop {
        let line = match read_line(&mut c).await {
            Ok(l) => l,
            Err(SamError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(());
            }
            Err(e) => {
                shared.violation(id, format!("unreadable command: {e}"));
                return Err(e.into());
            }
        };
        shared.state.lock().commands.push(line.clone());
        let cmd = match Reply::parse(&line) {
            Ok(c) => c,
            Err(e) => {
                shared.violation(id, format!("malformed command: {e}"));
                return Ok(());
            }
        };
        let violation = |what: String| shared.violation(id, what);
        let topic_kind = (cmd.topic.as_str(), cmd.kind.as_str());

        if !hello {
            if topic_kind != ("HELLO", "VERSION") {
                violation(format!("{line:?} before HELLO"));
                reply(&mut c, "HELLO REPLY", &[("RESULT", "I2P_ERROR")]).await?;
                return Ok(());
            }
            if let Err(e) = check_keys(&cmd, &["MIN", "MAX"], false) {
                violation(e);
            }
            let (bmin, bmax) = *shared.versions.lock();
            let parsed = |key: &str, default: Version| match cmd.get(key) {
                None => Some(default),
                Some(v) => parse_version(v),
            };
            let (Some(cmin), Some(cmax)) = (parsed("MIN", (3, 0)), parsed("MAX", (99, 0))) else {
                violation(format!("bad version in {line:?}"));
                reply(&mut c, "HELLO REPLY", &[("RESULT", "I2P_ERROR")]).await?;
                return Ok(());
            };
            let chosen = cmax.min(bmax);
            if chosen < cmin.max(bmin) {
                reply(&mut c, "HELLO REPLY", &[("RESULT", "NOVERSION")]).await?;
                return Ok(());
            }
            let v = format!("{}.{}", chosen.0, chosen.1);
            reply(&mut c, "HELLO REPLY", &[("RESULT", "OK"), ("VERSION", &v)]).await?;
            hello = true;
            continue;
        }

        match topic_kind {
            ("SESSION", "CREATE") => {
                if let Err(e) = check_keys(
                    &cmd,
                    &["STYLE", "ID", "DESTINATION", "SIGNATURE_TYPE"],
                    true,
                ) {
                    violation(e);
                }
                let fail = |why: String| {
                    violation(why.clone());
                    why
                };
                let checked = (|| {
                    if session_here.is_some() {
                        return Err(fail("a second SESSION CREATE on one socket".into()));
                    }
                    if cmd.get("STYLE") != Some("STREAM") {
                        return Err(fail(format!("STYLE={:?}", cmd.get("STYLE"))));
                    }
                    let sid = cmd.get("ID").unwrap_or_default();
                    if sid.is_empty() || sid.contains(char::is_whitespace) {
                        return Err(fail(format!("ID={sid:?}")));
                    }
                    if cmd.get("DESTINATION") != Some("TRANSIENT") {
                        return Err(fail(format!(
                            "DESTINATION={:?} (only TRANSIENT is expected)",
                            cmd.get("DESTINATION")
                        )));
                    }
                    if let Some(st) = cmd.get("SIGNATURE_TYPE")
                        && st.parse::<u16>().is_err()
                    {
                        return Err(fail(format!("SIGNATURE_TYPE={st:?}")));
                    }
                    for (k, v) in cmd.pairs() {
                        let range = match k.as_str() {
                            "inbound.quantity" | "outbound.quantity" => 1..=16,
                            "inbound.length" | "outbound.length" => 0..=7,
                            _ => continue,
                        };
                        if !v.parse::<u8>().is_ok_and(|n| range.contains(&n)) {
                            return Err(fail(format!("{k}={v:?} is out of {range:?}")));
                        }
                    }
                    Ok(sid.to_owned())
                })();
                let sid = match checked {
                    Ok(sid) => sid,
                    Err(why) => {
                        reply(
                            &mut c,
                            "SESSION STATUS",
                            &[("RESULT", "I2P_ERROR"), ("MESSAGE", &why)],
                        )
                        .await?;
                        continue;
                    }
                };
                let dest = random_destination();
                let created = {
                    let mut g = shared.state.lock();
                    if g.sessions.contains_key(&sid) {
                        false
                    } else {
                        let info = SessionInfo {
                            id: sid.clone(),
                            destination: dest.clone(),
                            options: cmd
                                .pairs()
                                .iter()
                                .filter(|(k, _)| {
                                    !matches!(k.as_str(), "STYLE" | "ID" | "DESTINATION")
                                })
                                .cloned()
                                .collect(),
                        };
                        g.history.push(info.clone());
                        g.sessions.insert(
                            sid.clone(),
                            BridgeSession {
                                info,
                                conn: id,
                                accepts: VecDeque::new(),
                                forwards: Vec::new(),
                                alive: CancellationToken::new(),
                            },
                        );
                        true
                    }
                };
                if !created {
                    reply(&mut c, "SESSION STATUS", &[("RESULT", "DUPLICATED_ID")]).await?;
                    continue;
                }
                session_here = Some(sid);
                // The private key: the destination followed by the private keys.
                let mut private = dest.as_bytes().to_vec();
                private.extend_from_slice(&rand::random::<[u8; 32]>());
                private.extend_from_slice(&rand::random::<[u8; 32]>());
                reply(
                    &mut c,
                    "SESSION STATUS",
                    &[
                        ("RESULT", "OK"),
                        ("DESTINATION", &i2p_base64_encode(&private)),
                    ],
                )
                .await?;
            }
            ("NAMING", "LOOKUP") => {
                if let Err(e) = check_keys(&cmd, &["NAME"], false) {
                    violation(e);
                }
                let Some(name) = cmd.get("NAME").map(str::to_owned) else {
                    violation("NAMING LOOKUP without NAME".into());
                    reply(&mut c, "NAMING REPLY", &[("RESULT", "INVALID_KEY")]).await?;
                    continue;
                };
                let found = if name == "ME" {
                    let g = shared.state.lock();
                    session_here
                        .as_ref()
                        .and_then(|sid| g.sessions.get(sid))
                        .map(|s| s.info.destination.clone())
                } else if let Some(hash) = I2pHash::from_b32_name(&name) {
                    shared.destination_by_hash(&hash)
                } else {
                    shared.state.lock().names.get(&name).cloned()
                };
                match found {
                    Some(d) => {
                        reply(
                            &mut c,
                            "NAMING REPLY",
                            &[
                                ("RESULT", "OK"),
                                ("NAME", &name),
                                ("VALUE", &d.to_i2p_base64()),
                            ],
                        )
                        .await?
                    }
                    None => {
                        reply(
                            &mut c,
                            "NAMING REPLY",
                            &[("RESULT", "KEY_NOT_FOUND"), ("NAME", &name)],
                        )
                        .await?
                    }
                }
            }
            ("STREAM", kind @ ("CONNECT" | "ACCEPT" | "FORWARD")) => {
                if session_here.is_some() {
                    violation(format!("STREAM {kind} on the socket of a session"));
                    reply(&mut c, "STREAM STATUS", &[("RESULT", "I2P_ERROR")]).await?;
                    return Ok(());
                }
                return match kind {
                    "CONNECT" => stream_connect(shared.clone(), id, c, cmd).await,
                    "ACCEPT" => stream_accept(shared.clone(), id, c, cmd).await,
                    _ => stream_forward(shared.clone(), id, c, cmd).await,
                };
            }
            _ => {
                violation(format!("unknown command {line:?}"));
                let words = format!("{} STATUS", cmd.topic);
                reply(&mut c, &words, &[("RESULT", "I2P_ERROR")]).await?;
            }
        }
    }
}

/// Copies both ways until either side closes or a session ends.
async fn splice(mut a: Conn, mut b: Conn, alive: [CancellationToken; 2]) {
    let [x, y] = alive;
    tokio::select! {
        _ = x.cancelled() => {}
        _ = y.cancelled() => {}
        r = tokio::io::copy_bidirectional(&mut a, &mut b) => {
            debug!("fake SAM stream ended: {r:?}");
        }
    }
}

async fn stream_connect(
    shared: Arc<Shared>,
    id: u64,
    mut c: Conn,
    cmd: Reply,
) -> anyhow::Result<()> {
    if let Err(e) = check_keys(&cmd, &["ID", "DESTINATION", "SILENT"], false) {
        shared.violation(id, e);
    }
    let silent = match check_silent(&cmd) {
        Ok(s) => s,
        Err(e) => {
            shared.violation(id, e);
            false
        }
    };
    async fn status(c: &mut Conn, result: &str, msg: Option<&str>) -> anyhow::Result<()> {
        let mut pairs = vec![("RESULT", result)];
        if let Some(msg) = msg {
            pairs.push(("MESSAGE", msg));
        }
        reply(c, "STREAM STATUS", &pairs).await
    }
    let sid = cmd.get("ID").unwrap_or_default().to_owned();
    let Some((from, from_alive)) = shared
        .state
        .lock()
        .sessions
        .get(&sid)
        .map(|s| (s.info.destination.clone(), s.alive.clone()))
    else {
        return status(&mut c, "INVALID_ID", None).await;
    };
    let target = match cmd.get("DESTINATION").map(I2pDestination::from_i2p_base64) {
        Some(Ok(d)) => d,
        _ => return status(&mut c, "INVALID_KEY", None).await,
    };
    let hash = target.hash();

    // A service hosted by the test.
    let service = {
        let mut g = shared.state.lock();
        g.services.get_mut(&hash).map(|s| {
            s.connections += 1;
            s.target
        })
    };
    if let Some(addr) = service {
        let Ok(tcp) = TcpStream::connect(addr).await else {
            return status(&mut c, "CANT_REACH_PEER", Some("the service is down")).await;
        };
        if !silent {
            status(&mut c, "OK", None).await?;
        }
        splice(c, BufReader::new(tcp), [from_alive.clone(), from_alive]).await;
        return Ok(());
    }

    // Another session: wait for it to accept.
    let deadline = tokio::time::Instant::now() + ACCEPT_WAIT;
    let mut incoming = Incoming {
        from,
        conn: c,
        silent,
        alive: from_alive,
    };
    loop {
        let mut notified = pin!(shared.accept_posted.notified());
        notified.as_mut().enable();
        enum Found {
            Accept(oneshot::Sender<Incoming>),
            Forward(Forward, CancellationToken),
            Nothing,
            NoSuchPeer,
        }
        let found = {
            let mut g = shared.state.lock();
            match g
                .sessions
                .values_mut()
                .find(|s| s.info.destination.hash() == hash)
            {
                None => Found::NoSuchPeer,
                Some(s) => {
                    let mut found = Found::Nothing;
                    while let Some(tx) = s.accepts.pop_front() {
                        if !tx.is_closed() {
                            found = Found::Accept(tx);
                            break;
                        }
                    }
                    if matches!(found, Found::Nothing)
                        && let Some(f) = s.forwards.first()
                    {
                        found = Found::Forward(f.clone(), s.alive.clone());
                    }
                    found
                }
            }
        };
        match found {
            Found::Accept(tx) => match tx.send(incoming) {
                Ok(()) => return Ok(()),
                // The acceptor went away meanwhile: the next one.
                Err(back) => incoming = back,
            },
            Found::Forward(f, to_alive) => {
                let mut c = incoming.conn;
                let Ok(mut tcp) = TcpStream::connect(f.target).await else {
                    return status(&mut c, "CANT_REACH_PEER", None).await;
                };
                if !f.silent {
                    let line = format!("{}\n", incoming.from.to_i2p_base64());
                    tcp.write_all(line.as_bytes()).await?;
                }
                if !silent {
                    status(&mut c, "OK", None).await?;
                }
                splice(c, BufReader::new(tcp), [incoming.alive, to_alive]).await;
                return Ok(());
            }
            Found::NoSuchPeer => {
                return status(
                    &mut incoming.conn,
                    "CANT_REACH_PEER",
                    Some("no such destination"),
                )
                .await;
            }
            Found::Nothing => {
                if tokio::time::timeout_at(deadline, notified).await.is_err() {
                    return status(
                        &mut incoming.conn,
                        "CANT_REACH_PEER",
                        Some("the peer does not accept"),
                    )
                    .await;
                }
            }
        }
    }
}

/// Waits until the client closes `c` (or sends something it must not).
async fn wait_closed_by_client(shared: &Shared, id: u64, c: &mut Conn, why: &str) {
    match c.fill_buf().await {
        Ok([]) | Err(_) => {}
        Ok(_) => shared.violation(id, format!("the client sent data on a socket that {why}")),
    }
}

async fn stream_accept(
    shared: Arc<Shared>,
    id: u64,
    mut c: Conn,
    cmd: Reply,
) -> anyhow::Result<()> {
    if let Err(e) = check_keys(&cmd, &["ID", "SILENT"], false) {
        shared.violation(id, e);
    }
    let silent = match check_silent(&cmd) {
        Ok(s) => s,
        Err(e) => {
            shared.violation(id, e);
            false
        }
    };
    let sid = cmd.get("ID").unwrap_or_default();
    let (tx, rx) = oneshot::channel();
    let alive = {
        let mut g = shared.state.lock();
        match g.sessions.get_mut(sid) {
            Some(s) => {
                s.accepts.push_back(tx);
                Some(s.alive.clone())
            }
            None => None,
        }
    };
    let Some(alive) = alive else {
        reply(&mut c, "STREAM STATUS", &[("RESULT", "INVALID_ID")]).await?;
        return Ok(());
    };
    reply(&mut c, "STREAM STATUS", &[("RESULT", "OK")]).await?;
    shared.accept_posted.notify_waiters();

    let incoming = tokio::select! {
        _ = alive.cancelled() => return Ok(()),
        _ = wait_closed_by_client(&shared, id, &mut c, "waits for a peer") => return Ok(()),
        r = rx => match r {
            Ok(i) => i,
            Err(_) => return Ok(()),
        },
    };
    let mut peer = incoming.conn;
    if !silent {
        let line = format!("{}\n", incoming.from.to_i2p_base64());
        c.get_mut().write_all(line.as_bytes()).await?;
    }
    if !incoming.silent {
        reply(&mut peer, "STREAM STATUS", &[("RESULT", "OK")]).await?;
    }
    splice(c, peer, [alive, incoming.alive]).await;
    Ok(())
}

async fn stream_forward(
    shared: Arc<Shared>,
    id: u64,
    mut c: Conn,
    cmd: Reply,
) -> anyhow::Result<()> {
    if let Err(e) = check_keys(&cmd, &["ID", "PORT", "HOST", "SILENT"], false) {
        shared.violation(id, e);
    }
    let silent = match check_silent(&cmd) {
        Ok(s) => s,
        Err(e) => {
            shared.violation(id, e);
            false
        }
    };
    let port = cmd.get("PORT").and_then(|p| p.parse::<u16>().ok());
    let host = match cmd.get("HOST") {
        None => Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        Some(h) => h.parse().ok(),
    };
    let (Some(port), Some(host)) = (port, host) else {
        shared.violation(
            id,
            format!("bad STREAM FORWARD target in {:?}", cmd.pairs()),
        );
        reply(&mut c, "STREAM STATUS", &[("RESULT", "I2P_ERROR")]).await?;
        return Ok(());
    };
    let sid = cmd.get("ID").unwrap_or_default();
    let alive = {
        let mut g = shared.state.lock();
        g.sessions.get_mut(sid).map(|s| {
            s.forwards.push(Forward {
                conn: id,
                target: SocketAddr::new(host, port),
                silent,
            });
            s.alive.clone()
        })
    };
    let Some(alive) = alive else {
        reply(&mut c, "STREAM STATUS", &[("RESULT", "INVALID_ID")]).await?;
        return Ok(());
    };
    reply(&mut c, "STREAM STATUS", &[("RESULT", "OK")]).await?;
    shared.accept_posted.notify_waiters();
    // The forward lasts as long as this socket (the caller removes it when it ends).
    tokio::select! {
        _ = alive.cancelled() => {}
        _ = wait_closed_by_client(&shared, id, &mut c, "forwards") => {}
    }
    Ok(())
}
