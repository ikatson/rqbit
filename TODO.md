- [x] Selective file downloading (mostly done)
  - [x] Proper counting of how much is left, and how much is downloaded

- [x] Send bitfield at the start if I have something
- [x] use the "update_hash" function in piece checking
- [ ] signaling when file is done

- [ ] per-file stats
- [ ] per-peer stats

- [ ] slow peers cause slowness in the end, need the "end of game" algorithm
  - [ ] will require implementing cancel message

someday:
- [ ] cancellation from the client-side for the lib (i.e. stop the torrent manager)


# concurrency
it's fucked up now, so need to rethink.

Sequencing:
- when the peer sends bitfield
  - update its bitfield
  - this can only happen at the start. But it can also NOT happen at all (if the peer wants to download)
    - so we actually cannot use it as a trigger to start the "uploader" of it (where we upload to it)
  - however both this and "have" we can use as a trigger to start the "downloader" part of it
- when the peer sends "interested"
- when the peer sends "have"
  - update its bitfield

"peer downloader":
- if started, means there was some initial interest
- if we are unchoked:
  - fetch new pieces forever from the queue, send requests