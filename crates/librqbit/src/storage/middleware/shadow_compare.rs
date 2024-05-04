use std::hash::Hasher;

use crate::storage::{StorageFactory, StorageFactoryExt, TorrentStorage};

#[derive(Clone)]
pub struct ShadowCompareStorageFactory<F1, F2> {
    primary: F1,
    mirror: F2,
}

impl<F1, F2> ShadowCompareStorageFactory<F1, F2> {
    pub fn new(primary: F1, mirror: F2) -> Self {
        Self { primary, mirror }
    }
}

impl<F1, F2> StorageFactory for ShadowCompareStorageFactory<F1, F2>
where
    F1: StorageFactory + Clone,
    F2: StorageFactory + Clone,
{
    type Storage = ShadowCompareStorage<F1::Storage, F2::Storage>;

    fn init_storage(&self, info: &crate::ManagedTorrentInfo) -> anyhow::Result<Self::Storage> {
        Ok(Self::Storage {
            primary: self.primary.init_storage(info)?,
            mirror: self.mirror.init_storage(info)?,
        })
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.clone().boxed()
    }
}

fn hash_buf(b: &[u8]) -> u64 {
    use std::hash::Hash;
    let mut h = std::hash::DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

pub struct ShadowCompareStorage<S1, S2> {
    primary: S1,
    mirror: S2,
}

impl<S1, S2> TorrentStorage for ShadowCompareStorage<S1, S2>
where
    S1: TorrentStorage,
    S2: TorrentStorage,
{
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        self.primary.pread_exact(file_id, offset, buf)?;
        let h1 = hash_buf(buf);
        self.mirror.pread_exact(file_id, offset, buf)?;
        let h2 = hash_buf(buf);
        if h1 != h2 {
            anyhow::bail!("corruption");
        }
        Ok(())
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        self.primary.pwrite_all(file_id, offset, buf)?;
        let h1 = hash_buf(buf);
        self.mirror.pwrite_all(file_id, offset, buf)?;
        let h2 = hash_buf(buf);
        if h1 != h2 {
            anyhow::bail!("corruption");
        }
        Ok(())
    }

    fn remove_file(&self, file_id: usize, filename: &std::path::Path) -> anyhow::Result<()> {
        self.primary.remove_file(file_id, filename)?;
        self.mirror.remove_file(file_id, filename)?;
        Ok(())
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.primary.ensure_file_length(file_id, length)?;
        self.mirror.ensure_file_length(file_id, length)?;
        Ok(())
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        Ok(Box::new(ShadowCompareStorage {
            primary: self.primary.take()?,
            mirror: self.mirror.take()?,
        }))
    }
}
