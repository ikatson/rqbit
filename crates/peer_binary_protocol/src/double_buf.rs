use std::io::IoSlice;

/// A helper for working with a buffer split into 2.
/// You can advance it forward (like you would do with buf=&buf[idx..])
#[derive(Clone, Copy)]
pub struct DoubleBufHelper<'a> {
    buf_0: &'a [u8],
    buf_1: &'a [u8],
}

impl<'a> DoubleBufHelper<'a> {
    pub fn new(buf: &'a [u8], buf2: &'a [u8]) -> Self {
        Self {
            buf_0: buf,
            buf_1: buf2,
        }
    }

    /// Consume len bytes and return them as 2 slices. Advances the buffer forward if successful.
    /// On error returns how many bytes are missing.
    pub fn consume_variable(&mut self, len: usize) -> Result<(&'a [u8], &'a [u8]), usize> {
        let available = self.buf_0.len() + self.buf_1.len();
        if available < len {
            return Err(len - available);
        }

        let first_len = self.buf_0.len().min(len);
        let (first_consumed, first_remaining) = self.buf_0.split_at(first_len);

        let second_len = (len - first_len).min(self.buf_1.len()); // the .min() here is just for split_at() to be optimized without panic
        let (second_consumed, second_remaining) = self.buf_1.split_at(second_len);

        self.buf_0 = first_remaining;
        self.buf_1 = second_remaining;

        Ok((first_consumed, second_consumed))
    }

    /// Read N bytes and advance the buffer by N if successful.
    /// Error returns how many missing bytes are there.
    pub fn consume<const N: usize>(&mut self) -> Result<[u8; N], usize> {
        match (self.buf_0.len(), self.buf_1.len()) {
            (l, _) if l >= N => {
                let (chunk, rem) = self.buf_0.split_at(N);
                self.buf_0 = rem;
                return Ok(chunk.try_into().unwrap());
            }
            (0, l) if l >= N => {
                let (chunk, rem) = self.buf_1.split_at(N);
                self.buf_1 = rem;
                return Ok(chunk.try_into().unwrap());
            }
            _ => {}
        }

        let mut res = [0u8; N];

        let first = self.buf_0.len().min(N);
        let second = self.buf_1.len().min(N.saturating_sub(first));

        let missing = N - first - second;
        if missing > 0 {
            return Err(missing);
        }

        res[..first].copy_from_slice(&self.buf_0[..first]);
        res[first..].copy_from_slice(&self.buf_1[..second]);
        self.buf_0 = &self.buf_0[first..];
        self.buf_1 = &self.buf_1[second..];
        Ok(res)
    }

    pub fn get(&self) -> [&'a [u8]; 2] {
        [self.buf_0, self.buf_1]
    }

    /// Read 4 big endian bytes and advance the buffer by 4 if successful.
    /// Error returns how many missing bytes are there.
    pub fn read_u32_be(&mut self) -> Result<u32, usize> {
        let data = self.consume::<4>()?;
        Ok(u32::from_be_bytes(data))
    }

    /// Read 1 byte and advance. Returns 1
    pub fn read_u8(&mut self) -> Option<u8> {
        let b = if !self.buf_0.is_empty() {
            &mut self.buf_0
        } else if !self.buf_1.is_empty() {
            &mut self.buf_1
        } else {
            return None;
        };
        let value = b[0];
        *b = &b[1..];
        Some(value)
    }

    /// Get a contiguous slice at the start if it exists.
    pub fn get_contiguous(&self, len: usize) -> Option<&'a [u8]> {
        match (self.buf_0.len(), self.buf_1.len()) {
            (l, _) if l >= len => Some(&self.buf_0[..len]),
            (0, l) if l >= len => Some(&self.buf_1[..len]),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        self.buf_0.len() + self.buf_1.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf_0.len() == 0 && self.buf_1.len() == 0
    }

    /// Advance it forward. If it was a single buffer this would be equivalent to buf=&buf[idx..]).
    /// If offset is too large, will set itself empty.
    pub fn advance(&mut self, offset: usize) {
        let buf_0_adv = self.buf_0.len().min(offset);
        self.buf_0 = &self.buf_0[buf_0_adv..];
        let buf_1_adv = (offset - buf_0_adv).min(self.buf_1.len());
        self.buf_1 = &self.buf_1[buf_1_adv..];
    }

    pub fn with_max_len(&self, max_len: usize) -> DoubleBufHelper<'a> {
        let buf_0_len = self.buf_0.len().min(max_len);
        let buf_1_len = (max_len - buf_0_len).min(self.buf_1.len());
        DoubleBufHelper {
            buf_0: &self.buf_0[..buf_0_len],
            buf_1: &self.buf_1[..buf_1_len],
        }
    }

    pub fn as_ioslices(&self, len_limit: usize) -> [IoSlice<'a>; 2] {
        let buf_0_len = self.buf_0.len().min(len_limit);
        let buf_1_len = (len_limit - buf_0_len).min(self.buf_1.len());
        [
            IoSlice::new(&self.buf_0[..buf_0_len]),
            IoSlice::new(&self.buf_1[..buf_1_len]),
        ]
    }
}

#[cfg(test)]
mod tests {
    use crate::double_buf::DoubleBufHelper;

    #[test]
    fn test_get_contiguous() {
        let d = DoubleBufHelper::new(&[], &[]);
        assert_eq!(d.get_contiguous(0).unwrap(), &[]);
        assert_eq!(d.get_contiguous(1), None);

        let d = DoubleBufHelper::new(&[42u8; 43], &[]);
        assert_eq!(d.get_contiguous(42).unwrap(), &[42u8; 42]);
        assert_eq!(d.get_contiguous(43).unwrap(), &[42u8; 43]);
        assert_eq!(d.get_contiguous(44), None);

        let d = DoubleBufHelper::new(&[], &[42u8; 43]);
        assert_eq!(d.get_contiguous(42).unwrap(), &[42u8; 42]);
        assert_eq!(d.get_contiguous(43).unwrap(), &[42u8; 43]);
        assert_eq!(d.get_contiguous(44), None);

        let d = DoubleBufHelper::new(&[42u8; 43], &[43u8; 52]);
        assert_eq!(d.get_contiguous(42).unwrap(), &[42u8; 42]);
        assert_eq!(d.get_contiguous(43).unwrap(), &[42u8; 43]);
        assert_eq!(d.get_contiguous(44), None);

        let d = DoubleBufHelper::new(&[], &[43u8; 52]);
        assert_eq!(d.get_contiguous(42).unwrap(), &[43u8; 42]);
        assert_eq!(d.get_contiguous(43).unwrap(), &[43u8; 43]);
        assert_eq!(d.get_contiguous(52).unwrap(), &[43u8; 52]);
        assert_eq!(d.get_contiguous(53), None);

        let d = DoubleBufHelper::new(&[42u8; 43], &[43u8; 52]);
        assert_eq!(d.get_contiguous(42).unwrap(), &[42u8; 42]);
    }

    #[test]
    fn test_consume() {
        for (first, second) in [(&[0, 1][..], &[][..]), (&[0], &[1]), (&[], &[0, 1])] {
            let mut d = DoubleBufHelper::new(first, second);
            assert_eq!(d.consume::<0>(), Ok([]));
            assert_eq!(d.len(), 2);

            let mut d = DoubleBufHelper::new(first, second);
            assert_eq!(d.consume::<1>(), Ok([0]));
            assert_eq!(d.len(), 1);

            let mut d = DoubleBufHelper::new(first, second);
            assert_eq!(d.consume::<2>(), Ok([0, 1]));
            assert_eq!(d.len(), 0);

            let mut d = DoubleBufHelper::new(first, second);
            assert_eq!(d.consume::<3>(), Err(1));
            assert_eq!(d.len(), 2);
        }
    }

    #[test]
    fn test_consume_variable() {
        let mut d = DoubleBufHelper::new(&[], &[]);
        assert_eq!(d.consume_variable(0), Ok((&[][..], &[][..])));
        assert_eq!(d.len(), 0);
        assert_eq!(d.consume_variable(1), Err(1));

        let mut d = DoubleBufHelper::new(&[0, 1], &[]);
        assert_eq!(d.consume_variable(0), Ok((&[][..], &[][..])));
        assert_eq!(d.len(), 2);

        let mut d = DoubleBufHelper::new(&[0, 1], &[]);
        assert_eq!(d.consume_variable(1), Ok((&[0][..], &[][..])));
        assert_eq!(d.len(), 1);
        assert_eq!(d.buf_0, &[1]);
        assert_eq!(d.buf_1, &[]);

        let mut d = DoubleBufHelper::new(&[0, 1], &[]);
        assert_eq!(d.consume_variable(2), Ok((&[0, 1][..], &[][..])));
        assert_eq!(d.len(), 0);
        assert_eq!(d.buf_0, &[]);
        assert_eq!(d.buf_1, &[]);

        let mut d = DoubleBufHelper::new(&[0, 1], &[]);
        assert_eq!(d.consume_variable(3), Err(1));
        assert_eq!(d.len(), 2);
        assert_eq!(d.buf_0, &[0, 1]);
        assert_eq!(d.buf_1, &[]);

        let mut d = DoubleBufHelper::new(&[0], &[1]);
        assert_eq!(d.consume_variable(0), Ok((&[][..], &[][..])));
        assert_eq!(d.len(), 2);

        let mut d = DoubleBufHelper::new(&[0], &[1]);
        assert_eq!(d.consume_variable(1), Ok((&[0][..], &[][..])));
        assert_eq!(d.len(), 1);
        assert_eq!(d.buf_0, &[]);
        assert_eq!(d.buf_1, &[1]);

        let mut d = DoubleBufHelper::new(&[0], &[1]);
        assert_eq!(d.consume_variable(2), Ok((&[0][..], &[1][..])));
        assert_eq!(d.len(), 0);
        assert_eq!(d.buf_0, &[]);
        assert_eq!(d.buf_1, &[]);

        let mut d = DoubleBufHelper::new(&[0], &[1]);
        assert_eq!(d.consume_variable(3), Err(1));
        assert_eq!(d.len(), 2);
        assert_eq!(d.buf_0, &[0]);
        assert_eq!(d.buf_1, &[1]);
    }

    #[test]
    fn test_advance_out_of_bounds_0() {
        let mut d = DoubleBufHelper::new(&[], &[]);
        d.advance(1);
        assert!(d.is_empty());
        assert_eq!(d.buf_0, &[]);
        assert_eq!(d.buf_1, &[]);
    }

    #[test]
    fn test_advance_out_of_bounds_1() {
        let mut d = DoubleBufHelper::new(&[42], &[]);
        d.advance(2);
        assert!(d.is_empty());
        assert_eq!(d.buf_0, &[]);
        assert_eq!(d.buf_1, &[]);
    }

    #[test]
    fn test_advance_out_of_bounds_2() {
        let mut d = DoubleBufHelper::new(&[42], &[43]);
        d.advance(3);
        assert!(d.is_empty());
        assert_eq!(d.buf_0, &[]);
        assert_eq!(d.buf_1, &[]);
    }

    #[test]
    fn test_advance_out_of_bounds_3() {
        let mut d = DoubleBufHelper::new(&[], &[42, 43]);
        d.advance(3);
        assert!(d.is_empty());
        assert_eq!(d.buf_0, &[]);
        assert_eq!(d.buf_1, &[]);
    }

    #[test]
    fn test_advance() {
        let mut d = DoubleBufHelper::new(&[], &[]);
        d.advance(0);
        assert_eq!(d.len(), 0);
        assert_eq!(d.consume_variable(1), Err(1));

        let mut d = DoubleBufHelper::new(&[0, 1], &[]);
        d.advance(0);
        assert_eq!(d.len(), 2);

        let mut d = DoubleBufHelper::new(&[0, 1], &[]);
        d.advance(1);
        assert_eq!(d.len(), 1);
        assert_eq!(d.buf_0, &[1]);
        assert_eq!(d.buf_1, &[]);

        let mut d = DoubleBufHelper::new(&[0, 1], &[]);
        d.advance(2);
        assert_eq!(d.len(), 0);
        assert_eq!(d.buf_0, &[]);
        assert_eq!(d.buf_1, &[]);

        let mut d = DoubleBufHelper::new(&[0], &[1]);
        d.advance(0);
        assert_eq!(d.len(), 2);

        let mut d = DoubleBufHelper::new(&[0], &[1]);
        d.advance(1);
        assert_eq!(d.len(), 1);
        assert_eq!(d.buf_0, &[]);
        assert_eq!(d.buf_1, &[1]);

        let mut d = DoubleBufHelper::new(&[0], &[1]);
        d.advance(2);
        assert_eq!(d.len(), 0);
        assert_eq!(d.buf_0, &[]);
        assert_eq!(d.buf_1, &[]);
    }
}
