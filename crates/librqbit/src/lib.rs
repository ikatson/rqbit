//!
//! This crate provides everything necessary to download [torrents](https://en.wikipedia.org/wiki/BitTorrent).
//!
//! # Quick usage example
//!
//! ```no_run
//! use librqbit::*;
//!
//! tokio_test::block_on(async {
//!     let session = Session::new("/tmp/where-to-download".into()).await.unwrap();
//!     let managed_torrent_handle = session.add_torrent(
//!        AddTorrent::from_url("magnet:?xt=urn:btih:cab507494d02ebb1178b38f2e9d7be299c86b862"),
//!        None // options
//!     ).await.unwrap().into_handle().unwrap();
//!     managed_torrent_handle.wait_until_completed().await.unwrap();
//! })
//! ```
//!
//! # Overview
//! The main type to start off with is [`Session`].
//!
//! It also proved useful to use the [`Api`] when building the rqbit desktop app, as it provides
//! a facade that works with simple serializable types.

pub mod api;
mod api_error;
mod chunk_tracker;
mod create_torrent_file;
mod dht_utils;
mod file_ops;
pub mod http_api;
pub mod http_api_client;
mod peer_connection;
mod peer_info_reader;
mod read_buf;
mod session;
mod spawn_utils;
mod torrent_state;
pub mod tracing_subscriber_config_utils;
mod type_aliases;

pub use api::Api;
pub use api_error::ApiError;
pub use create_torrent_file::{create_torrent, CreateTorrentOptions};
pub use dht;
pub use peer_connection::PeerConnectionOptions;
pub use session::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ListOnlyResponse, Session, SessionOptions,
    SUPPORTED_SCHEMES,
};
pub use spawn_utils::spawn as librqbit_spawn;
pub use torrent_state::{ManagedTorrent, ManagedTorrentState, TorrentStats, TorrentStatsState};

pub use buffers::*;
pub use clone_to_owned::CloneToOwned;
pub use librqbit_core::magnet::*;
pub use librqbit_core::peer_id::*;
pub use librqbit_core::torrent_metainfo::*;

#[cfg(test)]
mod tests;

/// The cargo version of librqbit.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn try_increase_nofile_limit() -> anyhow::Result<u64> {
    Ok(rlimit::increase_nofile_limit(1024 * 1024)?)
}
