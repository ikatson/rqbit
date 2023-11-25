- [ ] when we have the whole torrent, there's no point talking to peers that also have the whole torrent and keep reconnecting to them.
- [ ] per-file stats
- [x (partial)] per-peer stats
- [x] use some concurrent hashmap e.g. flurry or dashmap
- [x] tracing instead of logging. Debugging peers: RUST_LOG=[{peer=.*}]=debug
  test-log for tests
- [x] reopen read only is bugged
- [x] initializing/checking
  - [x] blocks the whole process. Need to break it up. On slower devices (rpi) just hangs for a good while
  - [x] checking torrents should be visible right away
- [ ] server persistence
  - [ ] it would be nice to restart the server and keep the state
- [x] torrent actions
  - [x] pause/unpause
  - [x] remove including from disk
- [ ] DHT
  - [ ] for torrents with a few seeds might be cool to re-query DHT once in a while.
  - [x] it's sending many requests now way too fast, locks up Mac OS UI annoyingly

someday:
- [x] cancellation from the client-side for the lib (i.e. stop the torrent manager)


refactor:
- [x] where are peers stored
- [x] http api pause/unpause etc
- [x] when a live torrent fails writing to disk, it should transition to error state
- [x] something is wrong when unpausing - can't finish. Recalculate needed/have from chunk tracker.
- [x] silence this: WARN torrent{id=0}:external_peer_adder: librqbit::spawn_utils: finished with error: no longer live

- [x] start from error state should be possible from UI
- [ ] if the torrent was completed, not need to re-check it
- [ ] checking is very slow on raspberry