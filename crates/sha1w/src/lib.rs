// Wrapper for sha1 libraries.
// Sha1 computation is the majority of CPU usage of this library.
// openssl seems 2-3x faster, so using it for now, but
// leaving the pure-rust impl here too. Maybe someday make them
// runtime swappable or enabled with a feature.

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
            inner: sha1::Sha1::new(),
        }
    }

    fn update(&mut self, buf: &[u8]) {
        self.inner.update(buf)
    }

    fn finish(self) -> [u8; 20] {
        self.inner.digest().bytes()
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
