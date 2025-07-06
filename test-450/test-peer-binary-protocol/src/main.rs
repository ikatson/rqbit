fn main() {
    librqbit_peer_protocol::Message::deserialize(
        std::hint::black_box(&[]),
        std::hint::black_box(&[]),
    )
    .unwrap();
}
