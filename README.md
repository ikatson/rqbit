# rqbit - bittorrent client in Rust

**rqbit** is a bittorrent client written in Rust.

## Motivation

First of all, I love Rust. The project was created purely for the fun of the process of writing code in Rust.

I was not satisfied with my regular bittorrent client, and was wondering how much work would it be to create a new one from scratch.

I got it to the point where it downloads torrents reliably and pretty fast, and I was using for a few months myself. It works good enough for me, and at the momnent of writing this I'm not planning to extend it further, as it works for me.

So in short, it's not "feature complete", but rather "good enough for me".

Open sourced it just in case anyone might find it useful and/or wants to contribute.

## Build

Just a regular Rust binary build process.

    cargo build --release

## Usage quick start

Assuming you are downloading to ~/Downloads

    rqbit 'magnet:?....' ~/Downloads

or

    rqbit /some/file.torrent ~/Downloads

## Useful options

### --list
Will print the contents of the torrent file or the magnet link.

### --overwrite
If you want to resume downloading a file that already exists, you'll need to add this option.

### --peer-connect-timeout=10s

This will increase the default peer connect timeout

### -r / --filename-re

Use a regex here to select files by their names.

## Features and missing features

### Some supported features
- Sequential downloading
- Resume downloading the same file
- Selective downloading (only certain files)
- DHT support. Allows magnet links to work, and also gets peers from the DHT in addition to trackers.
- HTTP API

### Code features
- Serde-based bencode serializer/deserializer
- Custom code for all binary protocol serialization/deserialization
- Custom code for everything else too :)
- Supports several SHA1 implementations, as this seems to be the biggest performance bottleneck. Default is openssl as it's the fastest in my benchmarks.
- In theory, the libraries that rqbit is made of are re-usable.

### Bugs, missing features and other caveats
Below points are all easily fixable, PRs welcome.

- If you try to run multiple instances, there's some port conflicts (already listening on port)
- The CLI support only one mode of operation: download one torrent to a given folder.
- HTTP API is rudimentary, mostly for looking at stats. E.g. you can't add a torrent through it.

## Code organization
- crates/rqbit - main binary
- crates/librqbit - main library
- crates/librqbit-core - torrent utils
- crates/bencode - bencode serializing/deserializing
- crates/buffers - wrappers around binary buffers
- crates/clone_to_owned - a trait to make something owned
- crates/sha1w - wrappers around sha1 libraries
- crates/peer_binary_protocol - the protocol to talk to peers
- crates/dht - Distributed Hash Table implementation

## HTTP API

By default it listens on http://127.0.0.1:3030, just curl it to see what methods are available.