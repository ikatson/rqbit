[![crates.io](https://img.shields.io/crates/v/rqbit.svg)](https://crates.io/crates/rqbit)
[![crates.io](https://img.shields.io/crates/v/librqbit.svg)](https://crates.io/crates/librqbit)
[![docs.rs](https://img.shields.io/docsrs/librqbit.svg)](https://docs.rs/crate/librqbit/latest)

# rqbit - bittorrent client in Rust

**rqbit** is a bittorrent client written in Rust.

## Installation

There are pre-built binaries in [releases](https://github.com/ikatson/rqbit/releases).
If someone wants to put rqbit into e.g. homebrew, PRs welcome :)

If you have rust toolchain installed, this should work:

```
cargo install rqbit
```

## Build
Just a regular Rust binary build process.

    cargo build --release

## Usage quick start

### Optional - start the server

Assuming you are downloading to ~/Downloads.

    rqbit server start ~/Downloads

### Download torrents

Assuming you are downloading to ~/Downloads. If the server is already started, ```-o ~/Downloads``` can be omitted.

    rqbit download -o ~/Downloads 'magnet:?....' [https?://url/to/.torrent] [/path/to/local/file.torrent]

## Useful options

### -v <log-level>
Increase verbosity. Possible values: trace, debug, info, warn, error.

### --list
Will print the contents of the torrent file or the magnet link.

### --overwrite
If you want to resume downloading a file that already exists, you'll need to add this option.

### --peer-connect-timeout=10s

This will increase the default peer connect timeout. The default one is 2 seconds, and it's sometimes not enough.

### -r / --filename-re

Use a regex here to select files by their names.

## Features and missing features

### Some supported features
- Sequential downloading (the default and only option)
- Resume downloading file(s) if they already exist on disk
- Selective downloading using a regular expression for filename
- DHT support. Allows magnet links to work, and makes more peers available.
- HTTP API
- Web UI

### Code features
- Serde-based bencode serializer/deserializer
- Custom code for binary protocol serialization/deserialization. And for everything else too :)
- Supports several SHA1 implementations, as this seems to be the biggest performance bottleneck. Default is openssl as it's the fastest in my benchmarks.
- In theory, the libraries that rqbit is made of are re-usable.
- No unsafe

### Bugs, missing features and other caveats
PRs are very welcome.

- Only supports BitTorrent V1 over TCP
- As this was created for personal needs, and for educational purposes, documentation, commit message quality etc. leave a lot to be desired.

## HTTP API

By default it listens on http://127.0.0.1:3030.

    curl -s 'http://127.0.0.1:3030/'

    {
        "apis": {
            "GET /": "list all available APIs",
            "GET /dht/stats": "DHT stats",
            "GET /dht/table": "DHT routing table",
            "GET /torrents": "List torrents (default torrent is 0)",
            "GET /torrents/{index}": "Torrent details",
            "GET /torrents/{index}/haves": "The bitfield of have pieces",
            "GET /torrents/{index}/stats": "Torrent stats",
            "POST /torrents/": "Add a torrent here. magnet: or http:// or a local file.",
            "GET /web/": "Web UI"
        }
    }

### Add torrent through HTTP API

```curl -d 'magnet:?...' http://127.0.0.1:3030/torrents```

OR

```curl -d 'http://.../file.torrent' http://127.0.0.1:3030/torrents```

OR

```curl -d '/some/local/file.torrent' http://127.0.0.1:3030/torrents```

Supported query parameters, all optional:
- overwrite=true|false
- only_files_regex - the regular expression string to match filenames
- output_folder - the folder to download to. If not specified, defaults to the one that rqbit server started with
- list_only=true|false - if you want to just list the files in the torrent instead of downloading

## Web UI
Access with http://localhost:3030/web/

<img width="1165" alt="Web UI" src="https://github.com/ikatson/rqbit/assets/221386/3854c1d6-94ee-4416-8af5-7ede6cd2047a">

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

## Motivation

First of all, I love Rust. The project was created purely for the fun of the process of writing code in Rust.

I was not satisfied with my regular bittorrent client, and was wondering how much work would it be to create a new one from scratch, and it got where it is, starting from bencode protocol implemenation, then peer protocol, etc, etc.