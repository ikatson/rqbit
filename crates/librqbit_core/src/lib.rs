pub mod constants;
pub mod directories;
pub mod hash_id;
pub mod lengths;
pub mod magnet;
pub mod peer_id;
pub mod spawn_utils;
pub mod speed_estimator;
pub mod torrent_metainfo;

pub use hash_id::Id20;

assert_cfg::exactly_one! {
    feature = "sha1-crypto-hash",
    feature = "sha1-ring",
}
