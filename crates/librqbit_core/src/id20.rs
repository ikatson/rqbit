use std::{cmp::Ordering, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Id20(pub [u8; 20]);

impl FromStr for Id20 {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut out = [0u8; 20];
        if s.len() != 40 {
            anyhow::bail!("expected a hex string of length 40")
        };
        hex::decode_to_slice(s, &mut out)?;
        Ok(Id20(out))
    }
}

impl std::fmt::Debug for Id20 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<")?;
        for byte in self.0 {
            write!(f, "{:02x?}", byte)?;
        }
        write!(f, ">")?;
        Ok(())
    }
}

impl Serialize for Id20 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(&self.0)
    }
}

impl<'de> Deserialize<'de> for Id20 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Id20;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(formatter, "a 20 byte slice or a 40 byte string")
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                if v.len() != 40 {
                    return Err(E::invalid_length(40, &self));
                }
                let mut out = [0u8; 20];
                match hex::decode_to_slice(v, &mut out) {
                    Ok(_) => Ok(Id20(out)),
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
                if v.len() != 20 {
                    return Err(E::invalid_length(20, &self));
                }
                let mut buf = [0u8; 20];
                buf.copy_from_slice(v);
                Ok(Id20(buf))
            }
        }
        deserializer.deserialize_any(Visitor {})
    }
}

impl Id20 {
    pub fn as_string(&self) -> String {
        hex::encode(self.0)
    }
    pub fn distance(&self, other: &Id20) -> Id20 {
        let mut xor = [0u8; 20];
        for (idx, (s, o)) in self
            .0
            .iter()
            .copied()
            .zip(other.0.iter().copied())
            .enumerate()
        {
            xor[idx] = s ^ o;
        }
        Id20(xor)
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

impl Ord for Id20 {
    fn cmp(&self, other: &Id20) -> Ordering {
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

impl PartialOrd<Id20> for Id20 {
    fn partial_cmp(&self, other: &Id20) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::Id20;

    #[test]
    fn test_set_bit_range() {
        let mut id = Id20([0u8; 20]);
        id.set_bits_range(9..17, true);
        assert_eq!(
            id,
            Id20([0, 127, 128, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        )
    }
}
