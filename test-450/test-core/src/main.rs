fn main() {
    dbg!(librqbit_core::torrent_metainfo::torrent_from_bytes(
        std::hint::black_box(&b"de"[..])
    ));
}
