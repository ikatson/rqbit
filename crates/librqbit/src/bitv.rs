use std::fs::File;

use anyhow::Context;
use bitvec::{
    boxed::BitBox,
    order::Msb0,
    slice::BitSlice,
    vec::BitVec,
    view::{AsBits, AsMutBits},
};

pub trait BitV: Send + Sync {
    fn as_slice(&self) -> &BitSlice<u8, Msb0>;
    fn as_slice_mut(&mut self) -> &mut BitSlice<u8, Msb0>;
    fn into_dyn(self) -> Box<dyn BitV>;
    fn as_bytes(&self) -> &[u8];
    fn flush(&mut self) -> anyhow::Result<()>;
}

pub type BoxBitV = Box<dyn BitV>;

pub struct MmapBitV {
    _file: File,
    mmap: memmap2::MmapMut,
}

impl MmapBitV {
    pub fn new(file: File) -> anyhow::Result<Self> {
        let mmap =
            unsafe { memmap2::MmapOptions::new().map_mut(&file) }.context("error mmapping file")?;
        Ok(Self { mmap, _file: file })
    }
}

#[async_trait::async_trait]
impl BitV for BitVec<u8, Msb0> {
    fn as_slice(&self) -> &BitSlice<u8, Msb0> {
        self.as_bitslice()
    }

    fn as_slice_mut(&mut self) -> &mut BitSlice<u8, Msb0> {
        self.as_mut_bitslice()
    }

    fn as_bytes(&self) -> &[u8] {
        self.as_raw_slice()
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn into_dyn(self) -> Box<dyn BitV> {
        Box::new(self)
    }
}

#[async_trait::async_trait]
impl BitV for BitBox<u8, Msb0> {
    fn as_slice(&self) -> &BitSlice<u8, Msb0> {
        self.as_bitslice()
    }

    fn as_slice_mut(&mut self) -> &mut BitSlice<u8, Msb0> {
        self.as_mut_bitslice()
    }

    fn as_bytes(&self) -> &[u8] {
        self.as_raw_slice()
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn into_dyn(self) -> Box<dyn BitV> {
        Box::new(self)
    }
}

impl BitV for MmapBitV {
    fn as_slice(&self) -> &BitSlice<u8, Msb0> {
        self.mmap.as_bits()
    }

    fn as_slice_mut(&mut self) -> &mut BitSlice<u8, Msb0> {
        self.mmap.as_mut_bits()
    }

    fn as_bytes(&self) -> &[u8] {
        &self.mmap
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(self.mmap.flush()?)
    }

    fn into_dyn(self) -> Box<dyn BitV> {
        Box::new(self)
    }
}

impl BitV for Box<dyn BitV> {
    fn as_slice(&self) -> &BitSlice<u8, Msb0> {
        (**self).as_slice()
    }

    fn as_slice_mut(&mut self) -> &mut BitSlice<u8, Msb0> {
        (**self).as_slice_mut()
    }

    fn as_bytes(&self) -> &[u8] {
        (**self).as_bytes()
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        (**self).flush()
    }

    fn into_dyn(self) -> Box<dyn BitV> {
        self
    }
}
