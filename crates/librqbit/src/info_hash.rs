pub type InfoHash = [u8; 20];

pub fn decode_info_hash(hash_str: &str) -> anyhow::Result<InfoHash> {
    let mut hash_arr = [0u8; 20];
    hex::decode_to_slice(hash_str, &mut hash_arr)?;
    Ok(hash_arr)
}
