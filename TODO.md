- [ ] Selective file downloading (mostly done)
  - [ ] Seeking optimization
    - If a file is not needed, no need to check its hash
  - [ ] Proper counting of how much is left, and how much is downloaded

- [ ] Refactor "needed pieces" into a bitfield
- [ ] Send bitfield at the start if I have something
- [ ] use the "update_hash" function in piece checking
- [ ] signaling when file is done


someday:
- [ ] cancellation