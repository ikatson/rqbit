use governor::InsufficientCapacity;
use peer_binary_protocol::MessageDeserializeError;
use tokio::sync::AcquireError;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("error connecting through proxy: {0:#}")]
    SocksConnect(
        #[from]
        #[source]
        tokio_socks::Error,
    ),
    #[error("error connecting over TCP: {0:#}")]
    TcpConnect(#[source] librqbit_dualstack_sockets::Error),
    #[error("error connecting over uTP: {0:#}")]
    UtpConnect(#[source] librqbit_utp::Error),
    #[error("can't connect, all connection methods disabled")]
    ConnectDisabled,
    #[error("error connecting: TCP={tcp:#} uTP={utp:#}")]
    Connect {
        tcp: librqbit_dualstack_sockets::Error,
        utp: librqbit_utp::Error,
    },
    #[error("uTP disabled")]
    UtpDisabled,
    #[error("TCP connections disabled")]
    TcpDisabled,

    #[error("wrong info hash")]
    WrongInfoHash,
    #[error("connecting to ourselves")]
    ConnectingToOurselves,

    #[error("error writing handshake: {0:#}")]
    WriteHandshake(#[source] std::io::Error),
    #[error("error reading handshake: {0:#}")]
    ReadHandshake(#[source] std::io::Error),

    #[error("error writing: {0:#}")]
    Write(#[source] std::io::Error),
    #[error("error reading: {0:#}")]
    Read(#[source] std::io::Error),

    #[error("timeout {0}")]
    Timeout(&'static str),

    #[error("peer disconnected while reading handshake")]
    PeerDisconnectedReadingHandshake,
    #[error("peer disconnected")]
    PeerDisconnected,

    #[error(transparent)]
    ProtoSerialize(#[from] peer_binary_protocol::SerializeError),

    #[error("error deserializing handshake: {0:#}")]
    DeserializeHandshake(#[source] MessageDeserializeError),
    #[error("error deserializing message: {0:#}")]
    Deserialize(
        #[from]
        #[source]
        MessageDeserializeError,
    ),

    #[error(transparent)]
    Anyhow(anyhow::Error),

    #[error("error reading chunk: {0:#}")]
    ReadChunk(#[source] anyhow::Error),

    #[error("disconnect requested")]
    Disconnect,

    #[error("disconnecting peer, reason: {0:#}")]
    DisconnectWithSource(#[source] anyhow::Error),

    #[error("bug: make_contiguous() called on a contiguous buffer; start={start} len={len}")]
    BugReadBufMakeContiguous { start: u16, len: u16 },

    #[error("read buffer is full. need_additional_bytes={need_additional_bytes}")]
    ReadBufFull { need_additional_bytes: u16 },

    #[cfg(test)]
    #[error("disconnected to simulate failure in tests")]
    TestDisconnect,

    #[error("torrent is not live")]
    TorrentIsNotLive,

    #[error("peer task is dead")]
    PeerTaskDead,

    #[error("chunk tracker empty, torrent was paused")]
    ChunkTrackerEmpty,

    #[error("rate limiting: insufficient capacity: {0:#}")]
    RateLimitInsufficientCapacity(
        #[from]
        #[source]
        InsufficientCapacity,
    ),

    #[error("semaphore closed")]
    SemaphoreAcquireError(
        #[from]
        #[source]
        AcquireError,
    ),

    #[error("bug: peer not found")]
    BugPeerNotFound,

    #[error("bug: invalid peer state")]
    BugInvalidPeerState,

    #[error("file is None, torrent was probably paused")]
    FsFileIsNone,

    #[error("session is dead")]
    SessionDestroyed,
}

pub type Result<T> = core::result::Result<T, Error>;
