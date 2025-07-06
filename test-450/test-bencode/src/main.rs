use librqbit_bencode::ByteBuf;

fn main() {
    librqbit_bencode::dyn_from_bytes::<ByteBuf>(&b"de"[..]).unwrap();
}
