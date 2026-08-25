//! 768-bit Diffie-Hellman key exchange for BitTorrent MSE.
//!
//! The MSE spec fixes a 768-bit MODP group (prime + generator 2) that is not
//! one of the standard groups shipped by crypto libraries (OpenSSL etc.), so
//! the group parameters are hardcoded here (matching libtorrent/transmission/
//! aria2, which do the same) and the modular exponentiation is delegated to
//! `crypto-bigint` (pure Rust, no C dependency, works with rqbit's `rust-tls`
//! build). The private exponent is 160 bits, matching common MSE peers.
//!
//! Note: `FixedMontyParams::new_vartime` is not constant-time. This is
//! acceptable here because the private exponent is a fresh ephemeral session
//! key generated per connection (not a long-lived secret); libtorrent's
//! non-openssl path uses boost `cpp_int::powm` (also variable-time) for the
//! same reason.

use crypto_bigint::{
    Odd, Uint,
    modular::{FixedMontyForm, FixedMontyParams},
};
use rand::Rng;

/// 768-bit modulus: 12 limbs of 64 bits.
type U768 = Uint<12>;

/// MSE-specified prime (RFC 2409-style 768-bit group, same hex as libtorrent
/// `pe_crypto.cpp` / transmission `peer-mse.cc` / aria2 `MSEHandshake.cc`).
const DH_PRIME_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1",
    "29024E088A67CC74020BBEA63B139B22514A08798E3404DD",
    "EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245",
    "E485B576625E7EC6F44C42E9A63A36210000000000090563"
);

const TWO: U768 = {
    let mut value = [0u64; 12];
    value[0] = 2;
    U768::from_words(value)
};

fn prime() -> U768 {
    U768::from_be_hex(DH_PRIME_HEX)
}

fn bytes_to_u768(bytes: &[u8; 96]) -> U768 {
    U768::from_be_slice(bytes)
}

fn u768_to_bytes(value: &U768) -> [u8; 96] {
    let encoded: [u8; 96] = value.to_be_bytes().into();
    encoded
}

fn powm(base: &U768, exponent: &[u8; 20]) -> U768 {
    let exp = secret_to_u768(exponent);
    powm_u(base, &exp)
}

fn secret_to_u768(secret: &[u8; 20]) -> U768 {
    U768::from_be_slice_truncated(secret, 160)
}

fn powm_u(base: &U768, exponent: &U768) -> U768 {
    let p_odd: Odd<U768> = Odd::new(prime()).expect("MSE prime is odd");
    // Variable-time Montgomery params: the private exponent is a fresh
    // per-connection ephemeral key (not a long-lived secret), so constant-time
    // is not required here; see the module note.
    let params = FixedMontyParams::new_vartime(p_odd);
    let monty_base = FixedMontyForm::new(base, &params);
    monty_base.pow(exponent).retrieve()
}

pub struct Dh768 {
    secret: [u8; 20],
    public: [u8; 96],
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
        let public = powm(&TWO, &secret);
        Self {
            secret,
            public: u768_to_bytes(&public),
        }
    }

    pub fn public_key_bytes(&self) -> [u8; 96] {
        self.public
    }

    pub fn shared_secret(&self, remote: &[u8; 96]) -> Option<[u8; 96]> {
        let remote = bytes_to_u768(remote);
        // Reject degenerate keys (0, 1, or >= p-1), matching the old hand-rolled
        // bounds check.
        let two = U768::from(2u64);
        let p = prime();
        let p_minus_one = p.wrapping_sub(&U768::ONE);
        if remote < two || remote >= p_minus_one {
            return None;
        }
        let secret_u = secret_to_u768(&self.secret);
        Some(u768_to_bytes(&powm_u(&remote, &secret_u)))
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
}
