#[cfg(test)]
mod tests {
    use bencode::ByteBuf;

    #[tokio::test]
    async fn it_works() {
        let data = b"64313a6164323a696432303abd7b477cfbcd10f30b705da20201e7101d8df155363a74617267657432303abd7b477cfbcd10f30b705da20201e7101d8df15565313a71393a66696e645f6e6f6465313a74323a0005313a79313a7165";
        let data = hex::decode(data).unwrap();
        dbg!(bencode::dyn_from_bytes::<ByteBuf>(data.as_slice()).unwrap());
    }
}
