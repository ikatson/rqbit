use librqbit_core::Id20;

#[derive(Clone, Copy, Debug)]
pub struct TorrentEvent {
    pub info_hash: Id20,
    pub kind: TorrentEventKind,
}

#[derive(Clone, Copy, Debug)]
pub enum TorrentEventKind {
    Added,
    Paused,
    Started,
    Deleted,
    Errored,
    Completed,
}

#[derive(Clone, Debug)]
pub struct SessionEventBus {
    event_tx: tokio::sync::broadcast::Sender<TorrentEvent>,
}

impl SessionEventBus {
    pub fn new() -> Self {
        let (event_tx, _) = tokio::sync::broadcast::channel(128);
        Self { event_tx }
    }
}

impl Default for SessionEventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
pub struct TorrentEventBus {
    info_hash: Id20,
    session_bus: SessionEventBus,
    event_tx: tokio::sync::broadcast::Sender<TorrentEventKind>,
}

impl SessionEventBus {
    pub(crate) fn new_torrent_bus(&self, info_hash: Id20) -> TorrentEventBus {
        let (event_tx, _) = tokio::sync::broadcast::channel(128);
        TorrentEventBus {
            info_hash,
            session_bus: self.clone(),
            event_tx,
        }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<TorrentEvent> {
        self.event_tx.subscribe()
    }
}

impl TorrentEventBus {
    pub(crate) fn emit(&self, event: TorrentEventKind) {
        let _ = self.event_tx.send(event);
        let _ = self.session_bus.event_tx.send(TorrentEvent {
            info_hash: self.info_hash,
            kind: event,
        });
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<TorrentEventKind> {
        self.event_tx.subscribe()
    }
}
