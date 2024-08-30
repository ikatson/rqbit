// Wrapper for sha1 libraries to be able to swap them easily,
// e.g. to measure performance, or change implementations depending on platform.
//
// Sha1 computation is the majority of CPU usage of librqbit.
// openssl is 2-3x faster than rust's sha1.
// system library is the best choice probably (it's the default anyway).

pub trait ISha1 {
    fn new() -> Self;
    fn update(&mut self, buf: &[u8]);
    fn finish(self) -> [u8; 20];
}

assert_cfg::exactly_one! {
    feature = "sha1-crypto-hash",
    feature = "sha1-ring",
}

#[cfg(feature = "sha1-crypto-hash")]
mod crypto_hash_impl {
    use super::ISha1;

    pub struct Sha1CryptoHash {
        inner: crypto_hash::Hasher,
    }

    impl ISha1 for Sha1CryptoHash {
        fn new() -> Self {
            Self {
                inner: crypto_hash::Hasher::new(crypto_hash::Algorithm::SHA1),
            }
        }

        fn update(&mut self, buf: &[u8]) {
            use std::io::Write;
            self.inner.write_all(buf).unwrap();
        }

        fn finish(mut self) -> [u8; 20] {
            let result = self.inner.finish();
            debug_assert_eq!(result.len(), 20);
            let mut result_arr = [0u8; 20];
            result_arr.copy_from_slice(&result);
            result_arr
        }
    }
}

#[cfg(feature = "sha1-ring")]
mod ring_impl {
    use super::ISha1;

    use aws_lc_rs::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY as SHA1};

    pub struct Sha1Ring {
        ctx: Context,
    }

    impl ISha1 for Sha1Ring {
        fn new() -> Self {
            Self {
                ctx: Context::new(&SHA1),
            }
        }

        fn update(&mut self, buf: &[u8]) {
            self.ctx.update(buf);
        }

        fn finish(self) -> [u8; 20] {
            let result = self.ctx.finish();
            debug_assert_eq!(result.as_ref().len(), 20);
            let mut result_arr = [0u8; 20];
            result_arr.copy_from_slice(result.as_ref());
            result_arr
        }
    }
}

#[cfg(feature = "sha1-crypto-hash")]
pub type Sha1 = crypto_hash_impl::Sha1CryptoHash;

#[cfg(feature = "sha1-ring")]
pub type Sha1 = ring_impl::Sha1Ring;
