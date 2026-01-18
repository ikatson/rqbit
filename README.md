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

Assuming you are downloading to ~/Downloads. By default it'll download to current directory.

    rqbit download [-o ~/Downloads] 'magnet:?....' [https?://url/to/.torrent] [/path/to/local/file.torrent]

## Web UI

Access at http://localhost:3030/web/. See screenshot below (torrent names and speeds are simulated).

<img width="1000" src="https://github.com/user-attachments/assets/d916b3d9-ebbd-462a-889d-df3916cc2681" />

## Desktop app

The desktop app is a [thin wrapper](https://github.com/ikatson/rqbit/blob/main/desktop/src-tauri/src/main.rs) on top of the Web UI frontend.

Download it in [Releases](https://github.com/ikatson/rqbit/releases) for OSX and Windows. For Linux, build manually with

    cargo tauri build

It looks similar to the Web UI (screenshot above).

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

## IPv6

rqbit supports IPv6. By default it listens on all interfaces in dualstack mode. It can work even if there's no IPv6 enabled.

## Shell completions

Assuming bash, add this to your `~/.bashrc`. Modify for your shell of choice.

```
eval "$(rqbit completions bash)"
```

## Socks proxy support

```
rqbit --socks-url socks5://[username:password]@host:port ...
```

## Watching a directory for .torrents

```
rqbit server start --watch-folder [path] /download/path
```

## Performance

Anecdotally from a few reports, rqbit is faster than other clients they've tried, at least with their default settings.

Memory usage for the server is usually within a few tens of megabytes, which makes it great for e.g. RaspberryPI.

I've got a report that rqbit can saturate a 20Gbps link, although I don't have the hardware to confirm.

## Installation

There are pre-built binaries in [Releases](https://github.com/ikatson/rqbit/releases).

[![](https://repology.org/badge/vertical-allrepos/rqbit.svg)](https://repology.org/project/rqbit/versions)

### Homebrew

**rqbit** can be installed using Homebrew.
```sh
brew install rqbit
```

### Cargo

If you have the Rust toolchain installed then you can use the following.
```sh
cargo install rqbit
```

## Docker

Docker images are published at [ikatson/rqbit](https://hub.docker.com/r/ikatson/rqbit)

## Build

Just a regular Rust binary build process.

    cargo build --release

The "webui" feature requires npm installed.

## Some useful options

Run ```rqbit --help``` to see all available CLI options.

### -v <log-level>

Increase verbosity. Possible values: trace, debug, info, warn, error.

### --list

Will print the contents of the torrent file or the magnet link.

### --overwrite

If you want to resume downloading a file that already exists, you'll need to add this option.

### -r / --filename-re

Use a regex here to select files by their names.

## Features (not exhaustive)

### Supported BEPs

- [BEP-3: The BitTorrent Protocol Specification](https://www.bittorrent.org/beps/bep_0003.html)
- [BEP-5: DHT Protocol](https://www.bittorrent.org/beps/bep_0005.html)
- [BEP-7: IPv6 Tracker Extension](https://www.bittorrent.org/beps/bep_0007.html)
- [BEP-9: Extension for Peers to Send Metadata Files](https://www.bittorrent.org/beps/bep_0009.html)
- [BEP-10: Extension Protocol](https://www.bittorrent.org/beps/bep_0010.html)
- [BEP-11: Peer Exchange (PEX)](https://www.bittorrent.org/beps/bep_0011.html)
- [BEP-12: Multitracker Metadata Extension](https://www.bittorrent.org/beps/bep_0012.html)
- [BEP-14: Local service discovery](https://www.bittorrent.org/beps/bep_0014.html)
- [BEP-15: UDP Tracker Protocol](https://www.bittorrent.org/beps/bep_0015.html)
- [BEP-20: Peer ID Conventions](https://www.bittorrent.org/beps/bep_0020.html)
- [BEP-23: Tracker Returns Compact Peer Lists](https://www.bittorrent.org/beps/bep_0023.html)
- [BEP-27: Private Torrents](https://www.bittorrent.org/beps/bep_0027.html)
- [BEP-29: uTorrent Transport Protocol](https://www.bittorrent.org/beps/bep_0029.html)
- [BEP-32: IPv6 extension for DHT](https://www.bittorrent.org/beps/bep_0032.html)
- [BEP-47: Padding files and extended file attributes](https://www.bittorrent.org/beps/bep_0047.html)
- [BEP-53: Magnet URI extension - Select specific file indices for download](https://www.bittorrent.org/beps/bep_0053.html)

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
- Download / upload rate limiting
- Prometheus metrics at ```/metrics``` and ```/torrents/<id_or_infohash>/peer_stats/prometheus```

## HTTP API

By default it listens on http://127.0.0.1:3030.

```
curl -s 'http://127.0.0.1:3030/'

{
  "apis": {
    "GET /": "list all available APIs",
    "GET /dht/stats": "DHT stats",
    "GET /dht/table": "DHT routing table",
    "GET /metrics": "Prometheus metrics",
    "GET /stats": "Global session stats",
    "GET /stream_logs": "Continuously stream logs",
    "GET /torrents": "List torrents",
    "GET /torrents/playlist": "Playlist for supported players",
    "GET /torrents/{id_or_infohash}": "Torrent details",
    "GET /torrents/{id_or_infohash}/haves": "The bitfield of have pieces",
    "GET /torrents/{id_or_infohash}/metadata": "Download the corresponding torrent file",
    "GET /torrents/{id_or_infohash}/peer_stats": "Per peer stats",
    "GET /torrents/{id_or_infohash}/peer_stats/prometheus": "Per peer stats in prometheus format",
    "GET /torrents/{id_or_infohash}/playlist": "Playlist for supported players",
    "GET /torrents/{id_or_infohash}/stats/v1": "Torrent stats",
    "GET /torrents/{id_or_infohash}/stream/{file_idx}": "Stream a file. Accepts Range header to seek.",
    "GET /web/": "Web UI",
    "POST /rust_log": "Set RUST_LOG to this post launch (for debugging)",
    "POST /torrents": "Add a torrent here. magnet: or http:// or a local file.",
    "POST /torrents/create": "Create a torrent and start seeding. Body should be a local folder",
    "POST /torrents/resolve_magnet": "Resolve a magnet to torrent file bytes",
    "POST /torrents/{id_or_infohash}/add_peers": "Add peers (newline-delimited)",
    "POST /torrents/{id_or_infohash}/delete": "Forget about the torrent, remove the files",
    "POST /torrents/{id_or_infohash}/forget": "Forget about the torrent, keep the files",
    "POST /torrents/{id_or_infohash}/pause": "Pause torrent",
    "POST /torrents/{id_or_infohash}/start": "Resume torrent",
    "POST /torrents/{id_or_infohash}/update_only_files": "Change the selection of files to download. You need to POST json of the following form {\"only_files\": [0, 1, 2]}"
  },
  "server": "rqbit",
  "version": "9.0.0-beta.1"
}
```

### Basic auth

For HTTP API basic authentication set RQBIT_HTTP_BASIC_AUTH_USERPASS environment variable.

```
RQBIT_HTTP_BASIC_AUTH_USERPASS=username:password rqbit server start ...
```

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
- [librqbit-utp](https://github.com/ikatson/librqbit-utp/) - uTP protocol
- [librqbit-dualstack-sockets](https://github.com/ikatson/librqbit-dualstack-sockets) - cross-platform IPv6+IPv4 listeners with canonical IPs

## Motivation

This project began purely out of my enjoyment of writing code in Rust. I wasn’t satisfied with my regular BitTorrent client and wanted to see how much effort it would take to build one from scratch. Starting with the bencode protocol, then the peer protocol, it gradually evolved into what it is today.

## Donations and sponsorship

If you love rqbit, please consider donating through one of these methods. With enough support, I might be able to make this my full-time job one day — which would be amazing!

- [Github Sponsors](https://github.com/sponsors/ikatson)
- Crypto
  - ETH (Ethereum) 0x68c54b26b5372d5f091b6c08cc62883686c63527
  - XMR (Monero) 49LcgFreJuedrP8FgnUVB8GkAyoPX7A9PjWfKZA1hNYz5vPCEcYQ9HzKr3pccGR6Lc3V3hn52bukwZShLDhZsk57V41c2ea
  - XNO (Nano) nano_1ghid3z6x41x8cuoffb6bbrt4e14wsqdbyqwp5d8rk166meo3h77q7mkjusr
