# Development guide

## Rust

Nothing special here. I run with

    make devserver

## Web UI

1. Start the server

    make devserver

2. Run Web UI dev

    make webui-dev

## Desktop app

1. Stop the devserver, otherwise ports will conflict.

2. Install deps

    cargo install tauri-cli
    make webui-deps

3. Run tauri dev

    cargo tauri dev