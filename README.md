[![crates.io](https://img.shields.io/crates/v/rqbit.svg)](https://crates.io/crates/rqbit)
[![crates.io](https://img.shields.io/crates/v/librqbit.svg)](https://crates.io/crates/librqbit)
[![docs.rs](https://img.shields.io/docsrs/librqbit.svg)](https://docs.rs/librqbit/latest/librqbit/)

# rqbit - bittorrent client in Rust

**rqbit** is a bittorrent client written in Rust. Has HTTP API and Web UI, and can be used as a library.

Also has a desktop app built with [Tauri](https://tauri.app/).

## Usage quick start

### Optional - start the server

Assuming you are downloading to ~/Downloads.

    rqbit server start ~/Downloads

### Download torrents

Assuming you are downloading to ~/Downloads. If the server is already started, `-o ~/Downloads` can be omitted.

    rqbit download -o ~/Downloads 'magnet:?....' [https?://url/to/.torrent] [/path/to/local/file.torrent]

## Web UI

Access with http://localhost:3030/web/. It looks similar to Desktop app, see screenshot below.

## Desktop app

The desktop app is a [thin wrapper](https://github.com/ikatson/rqbit/blob/main/desktop/src-tauri/src/main.rs) on top of the Web UI frontend.

Download it in [Releases](https://github.com/ikatson/rqbit/releases) for OSX and Windows. For Linux, build manually with

    cargo tauri build

<img width="1136" alt="Rqbit desktop" src="https://github.com/ikatson/rqbit/assets/221386/51f56542-667f-4f5e-a1e0-942b1df4cd5a">

## Streaming support

rqbit can stream torrent files and smartly block the stream until the pieces are available. The pieces getting streamed are prioritized. All of this allows you to seek and live stream videos for example.

You can also stream to e.g. VLC or other players with HTTP URLs. Supports seeking too (through various range headers).
The streaming URLs look like http://IP:3030/torrents/<torrent_id>/stream/<file_id>

## Integrated UPnP Media Server

rqbit can advertise managed torrents to LAN, e.g. your TVs and stream torrents there (without transcoding). Seeking to arbitrary points in the videos is supported too.

Usage from CLI

```
rqbit --enable-upnp-server server start ...
```

## Performance

Anecdotally from a few reports, rqbit is faster than other clients they've tried, at least with their default settings.

Memory usage for the server is usually within a few tens of megabytes, which makes it great for e.g. RaspberryPI.

CPU is spent mostly on SHA1 checksumming.

## Installation

There are pre-built binaries in [Releases](https://github.com/ikatson/rqbit/releases).
If someone wants to put rqbit into e.g. homebrew, PRs welcome :)

[![](https://repology.org/badge/vertical-allrepos/rqbit.svg)](https://repology.org/project/rqbit/versions)

If you have rust toolchain installed, this should work:

```
cargo install rqbit
```

## Docker

Docker images are published at [ikatson/rqbit](https://hub.docker.com/r/ikatson/rqbit)

## Build

Just a regular Rust binary build process.

    cargo build --release

The "webui" feature requires npm and python3 installed.

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
- Pausing / unpausing / deleting (with files or not) APIs
- Stateful server
- Web UI
- Streaming, with seeking
- UPNP port forwarding to your router
- UPNP Media Server
- Fastresume (no rehashing)

## HTTP API

By default it listens on http://127.0.0.1:3030.

    curl -s 'http://127.0.0.1:3030/'

    {
        "apis": {
            "GET /": "list all available APIs",
            "GET /dht/stats": "DHT stats",
            "GET /dht/table": "DHT routing table",
            "GET /torrents": "List torrents (default torrent is 0)",
            "GET /torrents/{id_or_infohash}": "Torrent details",
            "GET /torrents/{id_or_infohash}/haves": "The bitfield of have pieces",
            "GET /torrents/{id_or_infohash}/peer_stats": "Per peer stats",
            "GET /torrents/{id_or_infohash}/stats/v1": "Torrent stats",
            "GET /web/": "Web UI",
            "POST /rust_log": "Set RUST_LOG to this post launch (for debugging)",
            "POST /torrents": "Add a torrent here. magnet: or http:// or a local file.",
            "POST /torrents/{id_or_infohash}/delete": "Forget about the torrent, remove the files",
            "POST /torrents/{id_or_infohash}/forget": "Forget about the torrent, keep the files",
            "POST /torrents/{id_or_infohash}/pause": "Pause torrent",
            "POST /torrents/{id_or_infohash}/start": "Resume torrent",
            "POST /torrents/{id_or_infohash}/update_only_files": "Change the selection of files to download. You need to POST json of the following form {"only_files": [0, 1, 2]}"
        },
        "server": "rqbit"
    }

### Add torrent through HTTP API

`curl -d 'magnet:?...' http://127.0.0.1:3030/torrents`

OR

`curl -d 'http://.../file.torrent' http://127.0.0.1:3030/torrents`

OR

`curl --data-binary @/tmp/xubuntu-23.04-minimal-amd64.iso.torrent http://127.0.0.1:3030/torrents`

Supported query parameters, all optional:

- overwrite=true|false
- only_files_regex - the regular expression string to match filenames
- output_folder - the folder to download to. If not specified, defaults to the one that rqbit server started with
- list_only=true|false - if you want to just list the files in the torrent instead of downloading

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
- crates/upnp - upnp port forwarding
- crates/upnp_serve - upnp MediaServer
- desktop - desktop app built with [Tauri](https://tauri.app/)

## Motivation

First of all, I love Rust. The project was created purely for the fun of the process of writing code in Rust.

I was not satisfied with my regular bittorrent client, and was wondering how much work would it be to create a new one from scratch, and it got where it is, starting from bencode protocol implemenation, then peer protocol, etc, etc.
