pub mod compact_ip;
pub mod constants;
pub mod directories;
mod error;
pub mod hash_id;
pub mod lengths;
pub mod magnet;
pub mod merkle;
pub mod metainfo_v2;
pub mod peer_id;
pub mod spawn_utils;
pub mod speed_estimator;
pub mod torrent_metainfo;
pub mod web_seed;
pub use hash_id::{Id20, Id32};

pub use error::Error;
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(all(test, any(feature = "sha1-ring", feature = "sha1-crypto-hash")))]
mod bep52_tests;
