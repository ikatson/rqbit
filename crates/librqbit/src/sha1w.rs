// Wrapper for sha1 libraries.
// Sha1 computation is the majority of CPU usage of this library.
// openssl seems 2-3x faster, so using it for now, but
// leaving the pure-rust impl here too. Maybe someday make them
// runtime swappable.

pub trait ISha1 {
    fn new() -> Self;
    fn update(&mut self, buf: &[u8]);
    fn finish(self) -> [u8; 20];
}

pub struct Sha1Rust {
    inner: sha1::Sha1,
}

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
pub struct Sha1Openssl {
    inner: openssl::sha::Sha1,
}
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
