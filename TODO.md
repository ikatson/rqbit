- [ ] when we have the whole torrent, there's no point talking to peers that also have the whole torrent and keep reconnecting to them.
- [ ] per-file stats
- [x (partial)] per-peer stats
- [x] use some concurrent hashmap e.g. flurry or dashmap
- [x] tracing instead of logging. Debugging peers: RUST_LOG=[{peer=.*}]=debug
  test-log for tests
- [x] reopen read only is bugged
- [ ] initializing
  - [ ] blocks the whole process. Need to break it up. On slower devices (rpi) just hangs for a good while
  - [ ] initilizating torrents should be visible right away

someday:
- [ ] cancellation from the client-side for the lib (i.e. stop the torrent manager)