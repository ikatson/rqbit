- [x] when we have the whole torrent, there's no point talking to peers that also have the whole torrent and keep reconnecting to them.
- [ ] per-file stats
- [x (partial)] per-peer stats
- [x] use some concurrent hashmap e.g. flurry or dashmap
- [x] tracing instead of logging. Debugging peers: RUST_LOG=[{peer=.*}]=debug
  test-log for tests
- [x] reopen read only is bugged
- [x] initializing/checking
  - [x] blocks the whole process. Need to break it up. On slower devices (rpi) just hangs for a good while
  - [x] checking torrents should be visible right away
- [x] server persistence
  - [x] it would be nice to restart the server and keep the state
- [x] torrent actions
  - [x] pause/unpause
  - [x] remove including from disk
- [ ] DHT
  - [x] bootstrapping is lame
  - [x] many nodes in "Unknown" status, do smth about it
  - [x] for torrents with a few seeds might be cool to re-query DHT once in a while.
  - [x] don't leak memory when deleting torrents (i.e. remove torrent information (seen peers etc) once the torrent is deleted)
  - [ ] Buckets that have not been changed in 15 minutes should be "refreshed." (per RFC)
  - [x] it's sending many requests now way too fast, locks up Mac OS UI annoyingly
  - [ ] After the search is exhausted, the client then inserts the peer contact information for itself onto the responding nodes with IDs closest to the infohash of the torrent.
  - [x] Ensure that if we query the "returned" nodes, they are even closer to our request than the responding node id was.

someday:
- [x] cancellation from the client-side for the lib (i.e. stop the torrent manager)

- [x] favicons for Web UI

refactor:
- [ ] session persistence: should add torrents even if we haven't resolved it yet
- [x] where are peers stored
- [x] http api pause/unpause etc
- [x] when a live torrent fails writing to disk, it should transition to error state
- [x] something is wrong when unpausing - can't finish. Recalculate needed/have from chunk tracker.
- [x] silence this: WARN torrent{id=0}:external_peer_adder: librqbit::spawn_utils: finished with error: no longer live

- [x] start from error state should be possible from UI
- [x] checking is very slow on raspberry
  checked. nothing much can be done here. Even if raspberry's own libssl.so is used it's still super slow (sha1)
- [ ] .rqbit-session.json file has 0 bytes when disk full. I guess fs::rename does this when disk is full? at least on linux. Couldn't repro on MacOS