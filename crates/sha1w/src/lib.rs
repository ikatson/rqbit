// Wrapper for sha1 libraries to be able to swap them easily,
// e.g. to measure performance, or change implementations depending on platform.
//
// Sha1 computation is the majority of CPU usage of librqbit.
// openssl is 2-3x faster than rust's sha1.
// system library is the best choice probably (it's the default anyway).

pub type Sha1 = Sha1System;

pub trait ISha1 {
    fn new() -> Self;
    fn update(&mut self, buf: &[u8]);
    fn finish(self) -> [u8; 20];
}

pub struct Sha1System {
    inner: crypto_hash::Hasher,
}

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
