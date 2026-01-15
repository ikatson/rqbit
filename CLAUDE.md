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

# Desktop app. You cannot test it or see it, so don't bother running expensive "cargo tauri build"
cd desktop && npm install && tsc --noEmit
```

## Development Server

```bash
# Run test server that simulates traffic. Points to http://localhost:3030 for the main session's web UI and API.
# If you make changes to Rust this needs to be restarted.
make testserver

# Run webui in dev mode (hot reload vite server). Points to http://localhost:3031.
make webui-dev
```

@crates/librqbit/webui/CLAUDE.md has some details on webui if needed.

## Architecture

### Core Library (`crates/librqbit`)
The main library - the binary is just a thin CLI wrapper. Key components:

- **Session** (`session.rs`): Central coordinator managing torrents, DHT, peer connections, and persistence. Entry point for the library.
- **TorrentState** (`torrent_state/`): State machine for torrent lifecycle - initializing, live (downloading/seeding), paused
- **Storage** (`storage/`): Pluggable storage backends (filesystem, mmap) with middleware support (caching, timing)
- **HTTP API** (`http_api/`): REST API handlers for torrent management, streaming, DHT stats

### Supporting Crates
- `bencode` - Bencode serialization/deserialization
- `dht` - Distributed Hash Table (BEP-5)
- `peer_binary_protocol` - BitTorrent peer wire protocol
- `tracker_comms` - HTTP/UDP tracker communication
- `upnp` - Port forwarding
- `upnp-serve` - UPnP Media Server
- `librqbit_core` - Shared types (magnet links, torrent metainfo, peer IDs)
- `buffers` - Binary buffer utilities, small wrappers around bytes::Bytes and &[u8].
- `sha1w` - SHA1 wrapper (supports crypto-hash or openssl backends)

### Web UI (`crates/librqbit/webui`)
React + TypeScript + Tailwind CSS frontend. Shared between the HTTP API web interface and the Tauri desktop app.

### Desktop App (`desktop/`)
Tauri wrapper around the web UI.

## Other directives
- If you need to resort to running shell commands, always use "rg" instead of "grep".
- Prefer using Serena MCP instead of searching / reading / writing raw files when makes sense.
- `sed` on this machine is GNU sed (not BSD sed). Use `sed -i 's/foo/bar/g'` NOT `sed -i '' 's/foo/bar/g'`.
