- [ ] when we have the whole torrent, there's no point talking to peers that also have the whole torrent and keep reconnecting to them.
- [ ] per-file stats
- [ ] per-peer stats
- [ ] use some concurrent hashmap e.g. flurry or dashmap
- [x] tracing instead of logging. Debugging peers: RUST_LOG=[{peer=.*}]=debug
  test-log for tests

someday:
- [ ] cancellation from the client-side for the lib (i.e. stop the torrent manager)