- [ ] when we have the whole torrent, there's no point talking to peers that also have the whole torrent and keep reconnecting to them.
- [ ] per-file stats
- [x (partial)] per-peer stats
- [x] use some concurrent hashmap e.g. flurry or dashmap
- [x] tracing instead of logging. Debugging peers: RUST_LOG=[{peer=.*}]=debug
  test-log for tests
- [x] reopen read only is bugged
- [ ] initializing/checking
  - [ ] blocks the whole process. Need to break it up. On slower devices (rpi) just hangs for a good while
  - [ ] checking torrents should be visible right away
- [ ] server persistence
  - [ ] it would be nice to restart the server and keep the state
- [ ] torrent actions
  - [ ] pause/unpause
  - [ ] remove including from disk
- [ ] DHT
  - [ ] for torrents with a few seeds might be cool to re-query DHT once in a while
  - [ ] it's sending many requests now way too fast, locks up Mac OS UI annoyingly

someday:
- [ ] cancellation from the client-side for the lib (i.e. stop the torrent manager)