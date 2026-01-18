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

Access with http://localhost:3030/web/. It looks similar to the Desktop app, see screenshot below.

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

## List of CLI options and environment variables

| Cli option name | Environment name  | Default value | Other values | Description | 
|----------|----------|----------|----------|----------|
|  -v | RQBIT_LOG_LEVEL_CONSOLE  | info | trace, debug, info, warn, error |The console loglevel |
|  --log-file | RQBIT_LOG_FILE | | |The log filename to also write to in addition to the console |
| --log-file-rust-log | RQBIT_LOG_FILE_RUST_LOG | librqbit=debug,info | | The value for RUST_LOG in the log file |
| --i, --tracker-refresh-interval | RQBIT_TRACKER_REFRESH_INTERVAL | | | The interval to poll trackers, e.g. 30s. Trackers send the refresh interval when we connect to them. Often this is pretty big, e.g. 30 minutes. This can force a certain value. |
| --http-api-listen-addr | RQBIT_HTTP_API_LISTEN_ADDR | 127.0.0.1:3030 | | The listen address for HTTP API. If not set, "rqbit server" will listen on 127.0.0.1:3030, and "rqbit download" will listen on an ephemeral port that it will print. |
| --http-api-allow-create | RQBIT_HTTP_API_ALLOW_CREATE | | true, false | Allow creating torrents via HTTP API |
| | RQBIT_SINGLE_THREAD_RUNTIME | | true, false | Set this flag if you want to use tokio's single threaded runtime. It MAY perform better, but the main purpose is easier debugging, as time profilers work better with this one.|
| --disable-dht |RQBIT_DHT_DISABLE | | true, false| |
| --disable-dht-persistence | RQBIT_DHT_PERSISTENCE_DISABLE | | true, false | Set this to disable DHT reading and storing it's state. For now this is a useful workaround if you want to launch multiple rqbit instances, otherwise DHT port will conflict. |
| | RQBIT_SINGLE_THREAD_RUNTIME | | true, false | Set this flag if you want to use tokio's single threaded runtime. It MAY perform better, but the main purpose is easier debugging, as time profilers work better with this one. |
| --disable-dht | RQBIT_DHT_DISABLE | | true, false | |
| --disable-dht-persistence | RQBIT_DHT_PERSISTENCE_DISABLE | | true, false | Set this to disable DHT reading and storing it's state. For now this is a useful workaround if you want to launch multiple rqbit instances, otherwise DHT port will conflict.|
| --dht-bootstrap-addrs | RQBIT_DHT_BOOTSTRAP | | | Set DHT bootstrap addrs. A comma separated list of host:port or ip:port |
| --peer-connect-timeout | RQBIT_PEER_CONNECT_TIMEOUT | 2s | | The connect timeout, e.g. 1s, 1.5s, 100ms etc. |
| --peer-read-write-timeout | RQBIT_PEER_READ_WRITE_TIMEOUT | 10s | | The timeout for read() and write() operations, e.g. 1s, 1.5s, 100ms etc. |
| -t | RQBIT_RUNTIME_WORKER_THREADS | | | How many threads to spawn for the executor. |
| --disable-tcp-listen | RQBIT_TCP_LISTEN_DISABLE | |true, false | Disable listening for incoming connections over TCP. Note that outgoing connections can still be made (--disable-tcp-connect to disable). |
| --disable-tcp-connect | RQBIT_TCP_CONNECT_DISABLE | | true, false |  Disable outgoing connections over TCP. Note that listening over TCP for incoming connections is enabled by default (--disable-tcp-listen to disable) |
| --experimental-enable-utp-listen | RQBIT_EXPERIMENTAL_UTP_LISTEN_ENABLE | | true, false | Enable to listen and connect over uTP |
| --listen-port |RQBIT_LISTEN_PORT | 4240 | | The port to listen for incoming connections (applies to both TCP and uTP). Defaults to 4240 for the server, and an ephemeral port for "rqbit download / rqbit share" |
| --listen-ip | RQBIT_LISTEN_IP | :: | | What's the IP to listen on. Default is to listen on all interfaces on IPv4 and IPv6. |
| --disable-upnp-port-forward | RQBIT_UPNP_PORT_FORWARD_DISABLE | | true, false | By default, rqbit will try to publish LISTEN_PORT through UPnP on your router. This can disable it. |
| --enable-upnp-server | RQBIT_UPNP_SERVER_ENABLE | | true, false | If set, will run a UPnP Media server on RQBIT_HTTP_API_LISTEN_ADDR. |
| --upnp-server-friendly-name | RQBIT_UPNP_SERVER_FRIENDLY_NAME | | | UPnP server name that would be displayed on devices in your network |
| --bind-device | RQBIT_BIND_DEVICE | | | What network device to bind to for DHT, BT-UDP, BT-TCP, trackers and LSD. On OSX will use IP(V6)_BOUND_IF, on Linux will use SO_BINDTODEVICE. Not supported on Windows (will error if you try to use it). |
| --max-blocking-threads | RQBIT_RUNTIME_MAX_BLOCKING_THREADS | 8 | | How many maximum blocking tokio threads to spawn to process disk reads/writes. This will indicate how many parallel reads/writes can happen at a moment in time. The higher the number, the more the memory usage. |
| --defer-writes-up-to | RQBIT_DEFER_WRITES_UP_TO | | | If you set this to something, all writes to disk will happen in background and be buffered in memory up to approximately the given number of megabytes. Might be useful for slow disks. |
| | RQBIT_SOCKS_PROXY_URL | |`socks5://[username:password]@host:port` | If set will use socks5 proxy for all outgoing connections.You may also want to disable incoming connections via --disable-tcp-listen.|
| | RQBIT_CONCURRENT_INIT_LIMIT | 5 | | How many torrents can be initializing (rehashing) at the same time | 
| | RQBIT_UMASK | 022 | | Set the process umask to this value. Default is inherited from your environment (usually 022). This will affect the file mode of created files. Read more at https://man7.org/linux/man-pages/man2/umask.2.html |
| --disable-upload | RQBIT_DISABLE_UPLOAD | | true, false | Disable uploading entirely. If this is set, rqbit won't share piece availability and will disconnect on download request. Might be useful e.g. if rqbit upload consumes all your upload bandwidth and interferes with your other Internet usage. |
| --ratelimit-download | RQBIT_RATELIMIT_DOWNLOAD | | | Limit download speed to bytes-per-second. |
| --ratelimit-upload | RQBIT_RATELIMIT_UPLOAD | | | Limit upload speed to bytes-per-second. |
| | RQBIT_BLOCKLIST_URL | | | Downloads a p2p blocklist from this url and blocks connections from/to those peers. Supports file:/// and http(s):// URLs. Format is newline-delimited "name:start_ip-end_ip". E.g. https://github.com/Naunter/BT_BlockLists/raw/refs/heads/master/bt_blocklists.gz |
| | RQBIT_ALLOWLIST_URL | | | Downloads a p2p allowlist from this url and blocks ALL connections BUT from/to those peers. Supports file:/// and http(s):// URLs. Format is newline-delimited "name:start_ip-end_ip". E.g. https://github.com/Naunter/BT_BlockLists/raw/refs/heads/master/bt_blocklists.gz |
| | RQBIT_TRACKERS_FILENAME | | | The filename with tracker URLs to always use for each torrent. Newline-delimited. | 
| --disable-lsd | RQBIT_LSD_DISABLE | | true, false | Disable local peer discovery (LSD). By default rqbit will announce torrents to LAN |
| --disable-trackers | RQBIT_TRACKERS_DISABLE | | true, false | Disable trackers (for debugging DHT, LSD and --initial-peers) |
| --disable-persistence | RQBIT_SESSION_PERSISTENCE_DISABLE | | true, false | Disable server persistence. It will not read or write its state to disk. |
| --persistence-location | RQBIT_SESSION_PERSISTENCE_LOCATION | | | The folder to store session data in. By default uses OS specific folder. If starts with postgres://, will use postgres as the backend instead of JSON file. |
| --fastresume | RQBIT_FASTRESUME | | true, false | Experimental! if set, will try to resume quickly after restart and skip checksumming. |
| --watch-folder | RQBIT_WATCH_FOLDER | | | The folder to watch for added .torrent files. All files in this folder will be automatically added to the session. |
| | RQBIT_HTTP_BASIC_AUTH_USERPASS | | username:password | basic auth credentials should be in format username:password | 

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
