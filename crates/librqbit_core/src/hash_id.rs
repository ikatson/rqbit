use std::{cmp::Ordering, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Id<const N: usize>(pub [u8; N]);

impl<const N: usize> Id<N> {
    pub fn new(from: [u8; N]) -> Id<N> {
        Id(from)
    }

    pub fn as_string(&self) -> String {
        hex::encode(self.0)
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
        if s.len() != N*2 {
            anyhow::bail!("expected a hex string of length {}", N*2)
        };
        hex::decode_to_slice(s, &mut out)?;
        Ok(Id(out))
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
                formatter.write_str("a byte array of length ")
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

        deserializer.deserialize_any(IdVisitor{})
    }
}

impl<const N: usize> PartialOrd<Id<N>> for Id<N> {
    fn partial_cmp(&self, other: &Id<N>) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<const N: usize> Ord for Id<N> {
    fn cmp(&self, other: &Id<N>) -> Ordering {
        for (s, o) in self.0.iter().copied().zip(other.0.iter().copied()) {
            match s.cmp(&o) {
                Ordering::Less => return Ordering::Less,
                Ordering::Equal => continue,
                Ordering::Greater => return Ordering::Greater,
            }
        }
        Ordering::Equal
    }
}

/// A 20-byte hash used throughout librqbit, for torrent info hashes, peer ids etc.
pub type Id20 = Id<20>;
/// A 32-byte hash used in Bittorrent V2, for torrent info hashes, piece hashing, etc.
pub type Id32 = Id<32>;

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use super::*;

    #[test]
    fn test_set_bit_range() {
        let mut id = Id20::default();
        id.set_bits_range(9..17, true);
        assert_eq!(
            id,
            Id20::new([0, 127, 128, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        )
    }

    #[test]
    fn test_id32_from_str() {
        let str = "06f04cc728bef957a658876ef807f0514e4d715392969998efef584d2c3e435e";
        let _ih = Id32::from_str(str).unwrap();
    }

}