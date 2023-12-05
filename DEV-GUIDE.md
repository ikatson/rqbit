# Development guide

## Rust

Nothing special here. I run with

    make devserver

## Web UI

Start the server

    make devserver

Run Web UI dev

    make webui-dev

## Desktop app

Stop the devserver, otherwise ports will conflict.

Install deps

    cargo install tauri-cli
    make webui-deps

Run tauri dev

    cargo tauri dev