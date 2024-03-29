#![no_main]

use librqbit_core::torrent_metainfo::*;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| { if let Ok(_) = torrent_from_bytes::<TorrentMetaV1Borrowed>(data) {} });
