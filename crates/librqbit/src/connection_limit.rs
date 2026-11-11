//! A cap on peer connections (connecting or connected) that can be changed while connections
//! are open.
//!
//! Each torrent has one, and the session has one shared by all its torrents. A connection holds
//! a [`ConnectionPermit`] of both for as long as its task lives; the permit goes back when the
//! task ends, however it ends (connection tasks are cancelled by dropping them, so it is given
//! back on drop).
//!
//! Changing the cap is exact while permits are out. Raising it lets the waiting dialers go on at
//! once. Lowering it below the number of open connections disconnects nobody: the permits that
//! are out and over the new cap are retired as their connections end, and no new connection is
//! made until the count is below the cap.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use parking_lot::Mutex;
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};

struct Capacity {
    limit: Option<usize>,
    // Permits that are out but over the cap: retired instead of given back when they come back.
    // Always zero while the semaphore has permits available.
    debt: usize,
}

/// A resizable cap on concurrent connections. `None` is unlimited.
pub(crate) struct ConnectionLimit {
    // Fair (FIFO), so torrents waiting for the session budget take turns. "Unlimited" is the
    // largest capacity the semaphore can hold, so that the connections opened while unlimited
    // are counted when a cap is set later.
    semaphore: Arc<Semaphore>,
    capacity: Mutex<Capacity>,
    in_use: AtomicUsize,
}

fn permits_for(limit: Option<usize>) -> usize {
    limit.map_or(Semaphore::MAX_PERMITS, |l| l.min(Semaphore::MAX_PERMITS))
}

impl ConnectionLimit {
    pub(crate) fn new(limit: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            semaphore: Arc::new(Semaphore::new(permits_for(limit))),
            capacity: Mutex::new(Capacity { limit, debt: 0 }),
            in_use: AtomicUsize::new(0),
        })
    }

    /// The current cap. `None` is unlimited.
    pub(crate) fn limit(&self) -> Option<usize> {
        self.capacity.lock().limit
    }

    /// Changes the cap; see the module documentation for what happens to open connections.
    pub(crate) fn set_limit(&self, limit: Option<usize>) {
        let mut capacity = self.capacity.lock();
        let old = permits_for(capacity.limit);
        let new = permits_for(limit);
        if new > old {
            // Permits that are out over the old cap become legitimate first; whatever is left
            // is new room.
            let grow = new - old;
            let repaid = grow.min(capacity.debt);
            capacity.debt -= repaid;
            self.semaphore.add_permits(grow - repaid);
        } else if new < old {
            // Take what is available now; the rest is retired as it comes back.
            let shrink = old - new;
            let taken = self.semaphore.forget_permits(shrink);
            capacity.debt += shrink - taken;
        }
        capacity.limit = limit;
    }

    /// Waits for a permit. Waiters are served in order.
    pub(crate) async fn acquire(self: &Arc<Self>) -> Result<ConnectionPermit, AcquireError> {
        let permit = self.semaphore.clone().acquire_owned().await?;
        Ok(self.permit(permit))
    }

    /// A permit if one is available right now.
    pub(crate) fn try_acquire(self: &Arc<Self>) -> Option<ConnectionPermit> {
        let permit = self.semaphore.clone().try_acquire_owned().ok()?;
        Some(self.permit(permit))
    }

    fn permit(self: &Arc<Self>, permit: OwnedSemaphorePermit) -> ConnectionPermit {
        self.in_use.fetch_add(1, Ordering::Relaxed);
        ConnectionPermit {
            permit: Some(permit),
            limit: self.clone(),
        }
    }

    /// Whether another connection may start right now.
    #[cfg(test)]
    pub(crate) fn has_room(&self) -> bool {
        self.semaphore.available_permits() > 0
    }

    /// How many more connections may start right now.
    #[cfg(test)]
    pub(crate) fn available(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// How many permits are out: the connections connecting or connected, and dialers that hold
    /// a permit while they wait for another one.
    pub(crate) fn in_use(&self) -> usize {
        self.in_use.load(Ordering::Relaxed)
    }
}

/// One connection's share of a [`ConnectionLimit`], given back when dropped.
pub(crate) struct ConnectionPermit {
    permit: Option<OwnedSemaphorePermit>,
    limit: Arc<ConnectionLimit>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        let Some(permit) = self.permit.take() else {
            return;
        };
        // Under the lock, so that a concurrent change of the cap sees either the permit out or
        // given back, never neither.
        let mut capacity = self.limit.capacity.lock();
        if capacity.debt > 0 {
            capacity.debt -= 1;
            permit.forget();
        } else {
            drop(permit);
        }
        self.limit.in_use.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The permits a peer connection holds: its torrent's and the session's.
pub(crate) struct PeerPermits {
    _torrent: ConnectionPermit,
    _session: ConnectionPermit,
}

impl PeerPermits {
    /// Both permits if both limits have room right now (incoming connections are refused, not
    /// queued).
    pub(crate) fn try_acquire(
        torrent: &Arc<ConnectionLimit>,
        session: &Arc<ConnectionLimit>,
    ) -> Option<Self> {
        let torrent = torrent.try_acquire()?;
        let session = session.try_acquire()?;
        Some(Self {
            _torrent: torrent,
            _session: session,
        })
    }

    /// Waits for the torrent's permit, then for the session's.
    ///
    /// In this order, a torrent waits for the session budget with one dialer at most (its peer
    /// adder is sequential), so the session's FIFO queue serves the torrents in turn and a busy
    /// torrent can't starve the others. The other order would let a torrent whose own cap is
    /// full sit on session permits it can't use. It can't deadlock: a session permit is only ever
    /// held together with a torrent permit by a running connection, which waits for nothing.
    pub(crate) async fn acquire(
        torrent: &Arc<ConnectionLimit>,
        session: &Arc<ConnectionLimit>,
    ) -> Result<Self, AcquireError> {
        let torrent = torrent.acquire().await?;
        let session = session.acquire().await?;
        Ok(Self {
            _torrent: torrent,
            _session: session,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tokio::time::timeout;

    use super::{ConnectionLimit, ConnectionPermit, PeerPermits};

    fn take(limit: &Arc<ConnectionLimit>, n: usize) -> Vec<ConnectionPermit> {
        (0..n)
            .map(|_| limit.try_acquire().expect("a permit"))
            .collect()
    }

    #[test]
    fn test_permits_come_back_on_drop() {
        let limit = ConnectionLimit::new(Some(3));
        let permits = take(&limit, 3);
        assert!(limit.try_acquire().is_none());
        assert!(!limit.has_room());
        assert_eq!(limit.in_use(), 3);
        drop(permits);
        assert_eq!(limit.available(), 3);
        assert_eq!(limit.in_use(), 0);
    }

    #[test]
    fn test_lowering_below_the_open_count_retires_returning_permits() {
        let limit = ConnectionLimit::new(Some(5));
        let mut permits = take(&limit, 4);
        limit.set_limit(Some(2));
        assert_eq!(limit.limit(), Some(2));
        // Nobody is disconnected, and there is no room until two of the four have ended.
        assert_eq!(limit.in_use(), 4);
        assert!(!limit.has_room());
        permits.pop();
        assert!(!limit.has_room());
        permits.pop();
        assert!(!limit.has_room());
        permits.pop();
        assert_eq!(limit.available(), 1);
        permits.pop();
        assert_eq!(limit.available(), 2);
        assert_eq!(limit.in_use(), 0);
    }

    #[test]
    fn test_raising_repays_the_debt_first() {
        let limit = ConnectionLimit::new(Some(4));
        let permits = take(&limit, 4);
        limit.set_limit(Some(1));
        // Three over the cap: raising by two still leaves one over.
        limit.set_limit(Some(3));
        assert!(!limit.has_room());
        limit.set_limit(Some(6));
        assert_eq!(limit.available(), 2);
        drop(permits);
        assert_eq!(limit.available(), 6);
    }

    #[test]
    fn test_limit_set_on_an_unlimited_cap_counts_open_connections() {
        let limit = ConnectionLimit::new(None);
        assert_eq!(limit.limit(), None);
        let permits = take(&limit, 3);
        limit.set_limit(Some(2));
        assert!(!limit.has_room());
        drop(permits);
        assert_eq!(limit.available(), 2);
        limit.set_limit(None);
        assert!(limit.available() > 1 << 40);
        assert_eq!(limit.in_use(), 0);
    }

    #[test]
    fn test_unchanged_limit_is_a_no_op() {
        let limit = ConnectionLimit::new(Some(2));
        let _permit = limit.try_acquire();
        limit.set_limit(Some(2));
        assert_eq!(limit.available(), 1);
    }

    #[tokio::test]
    async fn test_raising_wakes_a_waiter() {
        let limit = ConnectionLimit::new(Some(0));
        let waiter = tokio::spawn({
            let limit = limit.clone();
            async move { limit.acquire().await.map(drop) }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        limit.set_limit(Some(1));
        timeout(Duration::from_secs(5), waiter)
            .await
            .expect("the waiter got the new permit")
            .expect("join")
            .expect("acquire");
        assert_eq!(limit.available(), 1);
    }

    #[test]
    fn test_refused_incoming_takes_no_permit() {
        let torrent = ConnectionLimit::new(Some(2));
        let session = ConnectionLimit::new(Some(1));
        let first = PeerPermits::try_acquire(&torrent, &session).expect("room in both");
        // The session cap is full: the torrent permit taken on the way is given back.
        assert!(PeerPermits::try_acquire(&torrent, &session).is_none());
        assert_eq!(torrent.available(), 1);
        drop(first);
        assert_eq!(torrent.available(), 2);
        assert_eq!(session.available(), 1);
    }

    #[tokio::test]
    async fn test_session_budget_is_served_in_turn() {
        // Two torrents wait for one session slot each time it frees up: they take turns, a
        // torrent that asks again goes to the back of the queue.
        let session = ConnectionLimit::new(Some(1));
        let a = ConnectionLimit::new(Some(10));
        let b = ConnectionLimit::new(Some(10));
        let held = PeerPermits::acquire(&a, &session).await.expect("acquire");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let dialer = |name: &'static str, torrent: Arc<ConnectionLimit>| {
            let session = session.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                for _ in 0..3 {
                    let permits = PeerPermits::acquire(&torrent, &session).await?;
                    if tx.send((name, permits)).is_err() {
                        break;
                    }
                }
                anyhow::Ok(())
            })
        };
        let da = dialer("a", a.clone());
        // Let a queue first.
        tokio::task::yield_now().await;
        let db = dialer("b", b.clone());
        tokio::task::yield_now().await;

        drop(held);
        let mut order = Vec::new();
        for _ in 0..6 {
            let (name, permits) = timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("a dialer got the slot")
                .expect("a dialer is alive");
            order.push(name);
            // The connection ends, the slot goes to the next in line.
            tokio::task::yield_now().await;
            drop(permits);
        }
        assert_eq!(order, ["a", "b", "a", "b", "a", "b"]);
        da.await.expect("join").expect("dialer a");
        db.await.expect("join").expect("dialer b");
        assert_eq!(session.available(), 1);
        assert_eq!(a.available(), 10);
        assert_eq!(b.available(), 10);
    }
}
