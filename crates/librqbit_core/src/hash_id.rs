use data_encoding::BASE32;
use serde::{Deserialize, Deserializer, Serialize};
use std::str::FromStr;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id<const N: usize>(pub [u8; N]);

impl<const N: usize> Id<N> {
    pub fn new(from: [u8; N]) -> Id<N> {
        Id(from)
    }

    pub fn as_string(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_bytes(b: &[u8]) -> anyhow::Result<Self> {
        let mut v = [0u8; N];
        if b.len() != N {
            anyhow::bail!("buffer length must be {}, but it's {}", N, b.len());
        }
        v.copy_from_slice(b);
        Ok(Id(v))
    }

    pub fn distance(&self, other: &Id<N>) -> Id<N> {
        let mut xor = [0u8; N];
        for (idx, (s, o)) in self
            .0
            .iter()
            .copied()
            .zip(other.0.iter().copied())
            .enumerate()
        {
            xor[idx] = s ^ o;
        }
        Id(xor)
    }
    pub fn get_bit(&self, bit: u8) -> bool {
        let n = self.0[(bit / 8) as usize];
        let mask = 1 << (7 - bit % 8);
        n & mask > 0
    }

    pub fn set_bit(&mut self, bit: u8, value: bool) {
        let n = &mut self.0[(bit / 8) as usize];
        if value {
            *n |= 1 << (7 - bit % 8)
        } else {
            let mask = !(1 << (7 - bit % 8));
            *n &= mask;
        }
    }
    pub fn set_bits_range(&mut self, r: std::ops::Range<u8>, value: bool) {
        for bit in r {
            self.set_bit(bit, value)
        }
    }
}

impl<const N: usize> Default for Id<N> {
    fn default() -> Self {
        Id([0; N])
    }
}

impl<const N: usize> std::fmt::Debug for Id<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{:02x?}", byte)?;
        }
        Ok(())
    }
}

impl<const N: usize> FromStr for Id<N> {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut out = [0u8; N];
        let base32_encoded_size = (N as f64 / 5f64).ceil() as usize * 8;
        if s.len() == N * 2 {
            hex::decode_to_slice(s, &mut out)?;
            Ok(Id(out))
            // try decode as base32
        } else if s.len() == base32_encoded_size {
            match BASE32.decode(s.as_bytes()) {
                Ok(decoded) => {
                    out.copy_from_slice(&decoded);
                    Ok(Id(out))
                }
                Err(err) => {
                    anyhow::bail!("fail to decode base32 string {}: {}", s, err)
                }
            }
        } else {
            anyhow::bail!(
                "expected a hex string of length {} or {}",
                N * 2,
                base32_encoded_size
            );
        }
    }
}

impl<const N: usize> Serialize for Id<N> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de, const N: usize> Deserialize<'de> for Id<N> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct IdVisitor<const N: usize>;

        impl<'de, const N: usize> serde::de::Visitor<'de> for IdVisitor<N> {
            type Value = Id<N>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter
                    .write_str("a byte array of length ")
                    .and_then(|_| formatter.write_fmt(format_args!("{}", N)))
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v.len() != N * 2 {
                    return Err(E::invalid_length(40, &self));
                }
                let mut out = [0u8; N];
                match hex::decode_to_slice(v, &mut out) {
                    Ok(_) => Ok(Id(out)),
                    Err(e) => Err(E::custom(e)),
                }
            }

            fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                self.visit_bytes(v)
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v.len() != N {
                    return Err(E::invalid_length(N, &self));
                }
                let mut buf = [0u8; N];
                buf.copy_from_slice(v);
                Ok(Id(buf))
            }
        }

        deserializer.deserialize_any(IdVisitor {})
    }
}

/// A 20-byte hash used throughout librqbit, for torrent info hashes, peer ids etc.
pub type Id20 = Id<20>;
/// A 32-byte hash used in Bittorrent V2, for torrent info hashes, piece hashing, etc.
pub type Id32 = Id<32>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_set_bit_range() {
        let mut id = Id20::default();
        id.set_bits_range(9..17, true);
        assert_eq!(
            id,
            Id20::new([
                0, 127, 128, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ])
        )
    }

    #[test]
    fn test_id32_from_str() {
        let str = "06f04cc728bef957a658876ef807f0514e4d715392969998efef584d2c3e435e";
        let _ih = Id32::from_str(str).unwrap();
    }

    #[test]
    fn test_id20_base32_encoded_from_str() {
        let str = "Z7QRDHYSJCA4U4HXGBXTFYUSDFGIRQMV";
        let ih1 = Id20::from_str(str).unwrap();
        let s2 = "cfe1119f124881ca70f7306f32e292194c88c195";
        let ih2 = Id20::from_str(s2).unwrap();
        assert_eq!(ih1, ih2);
    }
}
