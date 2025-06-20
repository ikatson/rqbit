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

    pub fn consume_variable(&mut self, len: usize) -> Result<(&'a [u8], &'a [u8]), usize> {
        let available = self.buf_0.len() + self.buf_1.len();
        if available < len {
            return Err(len - available);
        }

        let first_len = self.buf_0.len().min(len);
        let (first_consumed, first_remaining) = self.buf_0.split_at(first_len);

        let second_len = len - first_len;
        let (second_consumed, second_remaining) = self.buf_1.split_at(second_len);

        self.buf_0 = first_remaining;
        self.buf_1 = second_remaining;

        Ok((first_consumed, second_consumed))
    }

    pub fn consume<const N: usize>(&mut self) -> Result<[u8; N], usize> {
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

    pub fn read_u32_be(&mut self) -> Result<u32, usize> {
        let data = self.consume::<4>()?;
        Ok(u32::from_be_bytes(data))
    }

    pub fn read_u8(&mut self) -> Result<u8, usize> {
        let data = self.consume::<1>()?;
        Ok(data[0])
    }

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
}
