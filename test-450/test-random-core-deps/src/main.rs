use std::io::Write;

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let url = url::Url::parse(&args[1]).unwrap();
    dbg!(url);
    let mut hash = crypto_hash::Hasher::new(crypto_hash::Algorithm::SHA1);
    hash.write_all(args[1].as_bytes()).unwrap();
    dbg!(hash.finish());
}
