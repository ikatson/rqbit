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
            (l, 0) if l >= len => Some(&self.buf_0[..len]),
            (0, l) if l >= len => Some(&self.buf_1[..len]),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        self.buf_0.len() + self.buf_1.len()
    }
}
