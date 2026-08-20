//! RC4 (ARC4) stream cipher - self-contained, zero-dependency implementation
//! used by BitTorrent MSE (Message Stream Encryption) for full-stream
//! obfuscation.
//!
//! RC4 is cryptographically broken, but MSE requires it for wire-level
//! interoperability with Azureus/libtorrent/Thunder clients. The key is
//! typically the 20-byte SHA-1 output produced by the MSE key derivation.

/// RC4 state: a 256-entry permutation plus two index registers.
#[derive(Clone)]
pub struct Rc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    /// Key-scheduling algorithm (KSA). An empty key produces a deterministic
    /// identity state instead of panicking; MSE itself always supplies SHA-1
    /// keys, but keeping this constructor total makes misuse safe.
    pub fn new(key: &[u8]) -> Self {
        let mut s = [0u8; 256];
        for (idx, slot) in s.iter_mut().enumerate() {
            *slot = idx as u8;
        }

        if key.is_empty() {
            return Rc4 { s, i: 0, j: 0 };
        }

        let mut j: u8 = 0;
        for i in 0..256usize {
            j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
            s.swap(i, j as usize);
        }

        Rc4 { s, i: 0, j: 0 }
    }

    /// Pseudo-random generation algorithm (PRGA): XOR `buf` in place with the
    /// next `buf.len()` keystream bytes.
    pub fn apply_keystream(&mut self, buf: &mut [u8]) {
        for byte in buf.iter_mut() {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.s[self.i as usize]);
            self.s.swap(self.i as usize, self.j as usize);
            let k =
                self.s[(self.s[self.i as usize].wrapping_add(self.s[self.j as usize])) as usize];
            *byte ^= k;
        }
    }

    /// Discard the first `n` keystream bytes. MSE mandates discarding the first
    /// 1024 bytes after key setup (the "drop-1024" step).
    pub fn discard(&mut self, n: usize) {
        let mut scratch = [0u8; 256];
        let mut remaining = n;
        while remaining > 0 {
            let chunk = remaining.min(scratch.len());
            self.apply_keystream(&mut scratch[..chunk]);
            remaining -= chunk;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rc4_known_vectors() {
        let vectors: &[(&[u8], &[u8], &[u8])] = &[
            (
                b"Key",
                b"Plaintext",
                &[0xBB, 0xF3, 0x16, 0xE8, 0xD9, 0x40, 0xAF, 0x0A, 0xD3],
            ),
            (b"Wiki", b"pedia", &[0x10, 0x21, 0xBF, 0x04, 0x20]),
            (
                b"Secret",
                b"Attack at dawn",
                &[
                    0x45, 0xA0, 0x1F, 0x64, 0x5F, 0xC3, 0x5B, 0x38, 0x35, 0x52, 0x54, 0x4B, 0x9B,
                    0xF5,
                ],
            ),
        ];
        for (key, plaintext, expected) in vectors {
            let mut rc4 = Rc4::new(key);
            let mut buffer = plaintext.to_vec();
            rc4.apply_keystream(&mut buffer);
            assert_eq!(buffer.as_slice(), *expected);
        }
    }

    #[test]
    fn empty_key_is_safe_and_deterministic() {
        let mut first = Rc4::new(&[]);
        let mut second = Rc4::new(&[]);
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        first.apply_keystream(&mut a);
        second.apply_keystream(&mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn rc4_is_symmetric() {
        let key = [0x42u8; 20];
        let plaintext = b"the quick brown fox jumps over the lazy dog";
        let mut a = Rc4::new(&key);
        let mut enc = plaintext.to_vec();
        a.apply_keystream(&mut enc);
        assert_ne!(&enc[..], &plaintext[..]);

        let mut b = Rc4::new(&key);
        let mut dec = enc.clone();
        b.apply_keystream(&mut dec);
        assert_eq!(&dec[..], &plaintext[..]);
    }

    #[test]
    fn discard_advances_state() {
        let key = [0x42u8; 20];
        let mut with_discard = Rc4::new(&key);
        with_discard.discard(1024);

        let mut manual = Rc4::new(&key);
        let mut scratch = vec![0u8; 1024];
        manual.apply_keystream(&mut scratch);

        // After discarding 1024 bytes, the two states must produce identical
        // subsequent keystream.
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        with_discard.apply_keystream(&mut a);
        manual.apply_keystream(&mut b);
        assert_eq!(a, b);
    }
}
