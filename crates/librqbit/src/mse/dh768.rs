//! 768-bit Diffie-Hellman key exchange for BitTorrent MSE.
//!
//! This module uses fixed-size, little-endian limbs and has no big-integer
//! dependency. The private exponent is 160 bits, matching common MSE peers.

use rand::Rng;

type U768 = [u64; 12];
type U1536 = [u64; 24];

const DH_PRIME: U768 = [
    0x0000000000090563,
    0xF44C42E9A63A3621,
    0xE485B576625E7EC6,
    0x4FE1356D6D51C245,
    0x302B0A6DF25F1437,
    0xEF9519B3CD3A431B,
    0x514A08798E3404DD,
    0x020BBEA63B139B22,
    0x29024E088A67CC74,
    0xC4C6628B80DC1CD1,
    0xC90FDAA22168C234,
    0xFFFFFFFFFFFFFFFF,
];

const ONE: U768 = {
    let mut value = [0u64; 12];
    value[0] = 1;
    value
};

const TWO: U768 = {
    let mut value = [0u64; 12];
    value[0] = 2;
    value
};

fn bytes_be_to_limbs(bytes: &[u8; 96]) -> U768 {
    let mut limbs = [0u64; 12];
    for (i, limb) in limbs.iter_mut().enumerate() {
        let start = 96 - 8 * (i + 1);
        let mut be = [0u8; 8];
        be.copy_from_slice(&bytes[start..start + 8]);
        *limb = u64::from_be_bytes(be);
    }
    limbs
}

fn limbs_to_bytes_be(limbs: &U768) -> [u8; 96] {
    let mut bytes = [0u8; 96];
    for (i, limb) in limbs.iter().enumerate() {
        let start = 96 - 8 * (i + 1);
        bytes[start..start + 8].copy_from_slice(&limb.to_be_bytes());
    }
    bytes
}

fn ge(a: &U768, b: &U768) -> bool {
    for i in (0..12).rev() {
        if a[i] != b[i] {
            return a[i] > b[i];
        }
    }
    true
}

fn sub(a: &U768, b: &U768) -> (U768, bool) {
    let mut diff = [0u64; 12];
    let mut borrow = false;
    for i in 0..12 {
        let (first, borrow_first) = a[i].overflowing_sub(b[i]);
        let (second, borrow_second) = first.overflowing_sub(u64::from(borrow));
        diff[i] = second;
        borrow = borrow_first || borrow_second;
    }
    (diff, borrow)
}

fn mul(a: &U768, b: &U768) -> U1536 {
    let mut out = [0u64; 24];
    for i in 0..12 {
        let mut carry = 0u128;
        for j in 0..12 {
            let index = i + j;
            let value = out[index] as u128 + (a[i] as u128) * (b[j] as u128) + carry;
            out[index] = value as u64;
            carry = value >> 64;
        }
        let mut index = i + 12;
        while carry != 0 && index < out.len() {
            let value = out[index] as u128 + carry;
            out[index] = value as u64;
            carry = value >> 64;
            index += 1;
        }
    }
    out
}

/// Reduce a 1536-bit value while retaining the carry bit of the 769-bit
/// intermediate remainder. When the low-limb subtraction borrows, that borrow
/// is paid by the retained high bit instead of being treated as an error.
fn mod_reduce(value: &U1536, modulus: &U768) -> U768 {
    let mut remainder = [0u64; 12];
    let mut high = false;

    for bit in (0..1536).rev() {
        debug_assert!(!high);
        high = remainder[11] >> 63 != 0;
        let mut carry = 0u64;
        for limb in &mut remainder {
            let next_carry = *limb >> 63;
            *limb = (*limb << 1) | carry;
            carry = next_carry;
        }
        remainder[0] |= (value[bit / 64] >> (bit % 64)) & 1;

        if high || ge(&remainder, modulus) {
            let (difference, borrow) = sub(&remainder, modulus);
            remainder = difference;
            if high {
                high = !borrow;
            } else {
                debug_assert!(!borrow);
            }
        }
        debug_assert!(!high);
    }

    remainder
}

fn powm(base: &U768, exponent: &[u8; 20], modulus: &U768) -> U768 {
    let mut result = ONE;
    for byte in exponent {
        for bit in (0..8).rev() {
            result = mod_reduce(&mul(&result, &result), modulus);
            if (byte >> bit) & 1 != 0 {
                result = mod_reduce(&mul(&result, base), modulus);
            }
        }
    }
    result
}

pub struct Dh768 {
    secret: [u8; 20],
    public: U768,
}

impl Dh768 {
    pub fn generate(rng: &mut impl Rng) -> Self {
        let mut secret = [0u8; 20];
        while secret.iter().all(|byte| *byte == 0) {
            rng.fill_bytes(&mut secret);
        }
        Self::from_secret(secret)
    }

    pub(super) fn from_secret(secret: [u8; 20]) -> Self {
        let public = powm(&TWO, &secret, &DH_PRIME);
        Self { secret, public }
    }

    pub fn public_key_bytes(&self) -> [u8; 96] {
        limbs_to_bytes_be(&self.public)
    }

    pub fn shared_secret(&self, remote: &[u8; 96]) -> Option<[u8; 96]> {
        let remote = bytes_be_to_limbs(remote);
        if !ge(&remote, &TWO) {
            return None;
        }
        let prime_minus_one = sub(&DH_PRIME, &ONE).0;
        if ge(&remote, &prime_minus_one) {
            return None;
        }
        Some(limbs_to_bytes_be(&powm(&remote, &self.secret, &DH_PRIME)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, Result};
    use rand::SeedableRng;
    use rand::rngs::SmallRng;

    fn decode<const N: usize>(text: &str) -> Result<[u8; N]> {
        let bytes = hex::decode(text).context("invalid test vector hex")?;
        bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("test vector has the wrong length"))
    }

    #[test]
    fn matches_external_bigint_vectors() -> Result<()> {
        let secret_a = decode::<20>("000102030405060708090a0b0c0d0e0f10111213")?;
        let public_a = decode::<96>(
            "7fba71c678158bd55ef1cc04a919d1b05f79f9da403c67e82bb1a99a7b4bc4ec221cca6c3a78171a40f2cc12e3d9d4454338f7e4b9b33de5e82ab04e86f5cd43aaf9dad923988501c371d3159935de5499e5d726e740b1eabbf4a3dd03c68071",
        )?;
        let secret_b = decode::<20>("f0e0d0c0b0a09080706050403020100011223344")?;
        let public_b = decode::<96>(
            "f9fe7e1c27aee331ab8ff8a6183cfcc7bd08dc593fc4d52bc9a2694b7b787daa12e3b2695e3e9febf994447cefa427f9f5da34a4d3cd6c231a8d6517e7130de00a8a09e753ca12648ec18da389e68eeb66f8308b19cc60dfeaadb2540a821f53",
        )?;
        let shared = decode::<96>(
            "909ea4557d5b9f43dafdc5b598850045b8689e4d652af58a63730b00c574bbe4962ab9c78b2f295e3ddb3b456f20a4c65761751bf5d79ec4dba8470fe66ed22b4a25f13528a9575607c77586785a36d560f8556b66e9c16deb87fed185ee07a7",
        )?;

        let a = Dh768::from_secret(secret_a);
        let b = Dh768::from_secret(secret_b);
        assert_eq!(a.public_key_bytes(), public_a);
        assert_eq!(b.public_key_bytes(), public_b);
        assert_eq!(a.shared_secret(&public_b), Some(shared));
        assert_eq!(b.shared_secret(&public_a), Some(shared));
        Ok(())
    }

    #[test]
    fn generated_secret_is_nonzero() {
        let mut rng = SmallRng::seed_from_u64(0x5eed);
        let dh = Dh768::generate(&mut rng);
        assert!(dh.secret.iter().any(|byte| *byte != 0));
    }

    #[test]
    fn rejects_degenerate_remote_keys() {
        let dh = Dh768::from_secret([1u8; 20]);
        assert!(dh.shared_secret(&[0u8; 96]).is_none());
        assert!(dh.shared_secret(&[0xffu8; 96]).is_none());
        let mut one = [0u8; 96];
        one[95] = 1;
        assert!(dh.shared_secret(&one).is_none());
    }

    #[test]
    fn reduction_handles_769_bit_borrow() {
        let mut twice_prime = [0u64; 24];
        twice_prime[..12].copy_from_slice(&DH_PRIME);
        let mut carry = 0u64;
        for limb in &mut twice_prime[..12] {
            let next = *limb >> 63;
            *limb = (*limb << 1) | carry;
            carry = next;
        }
        twice_prime[12] = carry;
        assert_eq!(mod_reduce(&twice_prime, &DH_PRIME), [0u64; 12]);
    }
}
