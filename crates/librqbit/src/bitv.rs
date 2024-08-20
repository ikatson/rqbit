use std::fs::File;

use anyhow::Context;
use bitvec::{order::Lsb0, slice::BitSlice, vec::BitVec, view::AsBits, view::AsMutBits};

#[async_trait::async_trait]
pub trait BitV: Send {
    fn as_slice(&self) -> &BitSlice<u8, Lsb0>;
    fn as_slice_mut(&mut self) -> &mut BitSlice<u8, Lsb0>;
    fn into_dyn(self) -> Box<dyn BitV>;

    async fn flush(&mut self) -> anyhow::Result<()>;
}

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
impl BitV for BitVec<u8, Lsb0> {
    fn as_slice(&self) -> &BitSlice<u8, Lsb0> {
        self.as_bitslice()
    }

    fn as_slice_mut(&mut self) -> &mut BitSlice<u8, Lsb0> {
        self.as_mut_bitslice()
    }

    async fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn into_dyn(self) -> Box<dyn BitV> {
        Box::new(self)
    }
}

#[async_trait::async_trait]
impl BitV for MmapBitV {
    fn as_slice(&self) -> &BitSlice<u8, Lsb0> {
        self.mmap.as_bits()
    }

    fn as_slice_mut(&mut self) -> &mut BitSlice<u8, Lsb0> {
        self.mmap.as_mut_bits()
    }

    async fn flush(&mut self) -> anyhow::Result<()> {
        Ok(self.mmap.flush()?)
    }

    fn into_dyn(self) -> Box<dyn BitV> {
        Box::new(self)
    }
}

#[async_trait::async_trait]
impl BitV for Box<dyn BitV> {
    fn as_slice(&self) -> &BitSlice<u8, Lsb0> {
        (**self).as_slice()
    }

    fn as_slice_mut(&mut self) -> &mut BitSlice<u8, Lsb0> {
        (**self).as_slice_mut()
    }

    async fn flush(&mut self) -> anyhow::Result<()> {
        (**self).flush().await
    }

    fn into_dyn(self) -> Box<dyn BitV> {
        self
    }
}
