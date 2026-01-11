# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

rqbit is a BitTorrent client written in Rust with an HTTP API, Web UI, and desktop app (Tauri). The library (`librqbit`) can also be used standalone.

## Build Commands

```bash
# Build (release)
cargo build --release

# Build with webui feature (requires npm installed)
cargo build --release --features webui

# Run tests
cargo test                    # default members only
cargo test --workspace        # all workspace members

# Run a specific test
cargo test <test_name>
cargo test -p librqbit <test_name>   # test in specific crate

# Lint
cargo fmt --all -- --check
cargo clippy --all-targets

# Desktop app
cd desktop && npm install && cargo tauri build
```

## Development Server

```bash
# Install webui dependencies first
make webui-deps

# Run test server that simulates traffic
make testserver

# Run webui in dev mode (hot reload)
make webui-dev
```

Navigate to http://localhost:3031 for the vite server (hot reload) that points at the actual API backend at http://localhost:3030.

@crates/librqbit/webui/CLAUDE.md has some details on webui if needed.

## Architecture

### Core Library (`crates/librqbit`)
The main library - the binary is just a thin CLI wrapper. Key components:

- **Session** (`session.rs`): Central coordinator managing torrents, DHT, peer connections, and persistence. Entry point for the library.
- **TorrentState** (`torrent_state/`): State machine for torrent lifecycle - initializing, live (downloading/seeding), paused
- **Storage** (`storage/`): Pluggable storage backends (filesystem, mmap) with middleware support (caching, timing)
- **HTTP API** (`http_api/`): REST API handlers for torrent management, streaming, DHT stats
- **Peer Connection** (`peer_connection.rs`): BitTorrent peer protocol implementation

### Supporting Crates
- `bencode` - Bencode serialization/deserialization
- `dht` - Distributed Hash Table (BEP-5)
- `peer_binary_protocol` - BitTorrent peer wire protocol
- `tracker_comms` - HTTP/UDP tracker communication
- `upnp` - Port forwarding
- `upnp-serve` - UPnP Media Server
- `librqbit_core` - Shared types (magnet links, torrent metainfo, peer IDs)
- `buffers` - Binary buffer utilities
- `sha1w` - SHA1 wrapper (supports crypto-hash or ring backends)

### Web UI (`crates/librqbit/webui`)
React + TypeScript + Tailwind CSS frontend. Shared between the HTTP API web interface and the Tauri desktop app.

### Desktop App (`desktop/`)
Tauri wrapper around the web UI.

## Feature Flags (librqbit)

Key features:
- `http-api` - REST API (axum)
- `webui` - Embedded web UI
- `default-tls` / `rust-tls` - TLS backend selection
- `prometheus` - Metrics exporter
- `postgres` - PostgreSQL persistence backend
- `storage_middleware` - Storage middleware (caching)
- `watch` - Directory watching for .torrent files

## Environment Variables

Development server uses these (see Makefile):
- `RQBIT_HTTP_API_LISTEN_ADDR` - API listen address (default: `[::]:3030`)
- `RQBIT_LOG_FILE` - Log file path
- `RQBIT_LOG_FILE_RUST_LOG` - Log level for file
- `RQBIT_HTTP_BASIC_AUTH_USERPASS` - Basic auth (`username:password`)

## Testing Notes

- Tests run on Windows, macOS, and Linux
- macOS may need `ulimit -n unlimited` before running tests
- Minimum supported Rust version: 1.90
