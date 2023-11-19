// Wrapper for sha1 libraries to be able to swap them easily,
// e.g. to measure performance, or change implementations depending on platform.
//
// Sha1 computation is the majority of CPU usage of librqbit.
// openssl is 2-3x faster than rust's sha1.
// system library is the best choice probably (it's the default anyway).

#[cfg(feature = "sha1-openssl")]
pub type Sha1 = Sha1Openssl;

#[cfg(feature = "sha1-rust")]
pub type Sha1 = Sha1Rust;

#[cfg(feature = "sha1-system")]
pub type Sha1 = Sha1System;

pub trait ISha1 {
    fn new() -> Self;
    fn update(&mut self, buf: &[u8]);
    fn finish(self) -> [u8; 20];
}

#[cfg(feature = "sha1-rust")]
pub struct Sha1Rust {
    inner: sha1::Sha1,
}

#[cfg(feature = "sha1-rust")]
impl ISha1 for Sha1Rust {
    fn new() -> Self {
        Sha1Rust {
            inner: sha1::Sha1::default(),
        }
    }

    fn update(&mut self, buf: &[u8]) {
        use sha1::Digest;
        sha1::Sha1::update(&mut self.inner, buf)
    }

    fn finish(self) -> [u8; 20] {
        use sha1::Digest;
        let mut output = [0u8; 20];
        sha1::Sha1::finalize_into(self.inner, (&mut output[..]).into());
        output
    }
}

#[cfg(feature = "sha1-openssl")]
pub struct Sha1Openssl {
    inner: openssl::sha::Sha1,
}

#[cfg(feature = "sha1-openssl")]
impl ISha1 for Sha1Openssl {
    fn new() -> Self {
        Self {
            inner: openssl::sha::Sha1::new(),
        }
    }

    fn update(&mut self, buf: &[u8]) {
        self.inner.update(buf)
    }

    fn finish(self) -> [u8; 20] {
        self.inner.finish()
    }
}

#[cfg(feature = "sha1-system")]
pub struct Sha1System {
    inner: crypto_hash::Hasher,
}

#[cfg(feature = "sha1-system")]
impl ISha1 for Sha1System {
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
