- [ ] when we have the whole torrent, there's no point talking to peers that also have the whole torrent and keep reconnecting to them.
- [ ] per-file stats
- [x (partial)] per-peer stats
- [x] use some concurrent hashmap e.g. flurry or dashmap
- [x] tracing instead of logging. Debugging peers: RUST_LOG=[{peer=.*}]=debug
  test-log for tests
- [ ] reopen read only is bugged:
  expected to be able to write to disk: error writing to file 0 (""The.Creator.2023.D.AMZN.WEB-DLRip.1.46Gb.MegaPeer.avi"")

someday:
- [ ] cancellation from the client-side for the lib (i.e. stop the torrent manager)