use bitvec::{order::Lsb0, vec::BitVec};

use crate::{api::TorrentIdOrHash, bitv::BitV};

#[async_trait::async_trait]
pub trait BitVFactory: Send {
    async fn load(&self, id: TorrentIdOrHash) -> anyhow::Result<Option<Box<dyn BitV>>>;
    async fn store_initial_check(
        &self,
        id: TorrentIdOrHash,
        b: BitVec<u8, Lsb0>,
    ) -> anyhow::Result<Box<dyn BitV>>;
}

pub struct NonPersistentBitVFactory {}

#[async_trait::async_trait]
impl BitVFactory for NonPersistentBitVFactory {
    async fn load(&self, _: TorrentIdOrHash) -> anyhow::Result<Option<Box<dyn BitV>>> {
        Ok(None)
    }
    async fn store_initial_check(
        &self,
        id: TorrentIdOrHash,
        b: BitVec<u8, Lsb0>,
    ) -> anyhow::Result<Box<dyn BitV>> {
        Ok(Box::new(b))
    }
}
