pub mod filesystem;

#[cfg(feature = "storage_examples")]
pub mod examples;

#[cfg(feature = "storage_middleware")]
pub mod middleware;

use std::{
    any::{Any, TypeId},
    path::Path,
};

use anyhow::Context;
use librqbit_core::lengths::ValidPieceIndex;

use crate::{torrent_state::ManagedTorrentInfo, FileInfos};

pub trait StorageFactory: Send + Sync + Any {
    type Storage: TorrentStorage;

    fn init_storage(&self, info: &ManagedTorrentInfo) -> anyhow::Result<Self::Storage>;
    fn is_type_id(&self, type_id: TypeId) -> bool {
        Self::type_id(self) == type_id
    }
    fn clone_box(&self) -> BoxStorageFactory;
}

pub type BoxStorageFactory = Box<dyn StorageFactory<Storage = Box<dyn TorrentStorage>>>;

pub trait StorageFactoryExt {
    fn boxed(self) -> BoxStorageFactory;
}

impl<SF: StorageFactory> StorageFactoryExt for SF {
    fn boxed(self) -> BoxStorageFactory {
        struct Wrapper<SF> {
            sf: SF,
        }

        impl<SF: StorageFactory> StorageFactory for Wrapper<SF> {
            type Storage = Box<dyn TorrentStorage>;

            fn init_storage(&self, info: &ManagedTorrentInfo) -> anyhow::Result<Self::Storage> {
                let s = self.sf.init_storage(info)?;
                Ok(Box::new(s))
            }

            fn is_type_id(&self, type_id: TypeId) -> bool {
                self.sf.type_id() == type_id
            }

            fn clone_box(&self) -> BoxStorageFactory {
                self.sf.clone_box()
            }
        }

        Box::new(Wrapper { sf: self })
    }
}

impl<U: StorageFactory + ?Sized> StorageFactory for Box<U> {
    type Storage = U::Storage;

    fn init_storage(&self, info: &ManagedTorrentInfo) -> anyhow::Result<U::Storage> {
        (**self).init_storage(info)
    }

    fn clone_box(&self) -> BoxStorageFactory {
        (**self).clone_box()
    }
}

pub trait TorrentStorage: Send + Sync {
    /// Given a file_id (which you can get more info from in init_storage() through torrent info)
    /// read buf.len() bytes into buf at offset.
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()>;

    /// Given a file_id (which you can get more info from in init_storage() through torrent info)
    /// write buf.len() bytes into the file at offset.
    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()>;

    /// Remove a file from the storage. If not supported, or it doesn't matter, just return Ok(())
    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()>;

    /// E.g. for filesystem backend ensure that the file has a certain length, and grow/shrink as needed.
    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()>;

    fn flush_piece(&self, _piece_id: ValidPieceIndex) -> anyhow::Result<()> {
        Ok(())
    }

    /// Replace the current storage with a dummy, and return a new one that should be used instead.
    /// This is used to make the underlying object useless when e.g. pausing the torrent.
    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>>;

    // fn pread_exact_absolute(
    //     &self,
    //     absolute_offset: u64,
    //     mut buf: &mut [u8],
    //     file_infos: &FileInfos,
    // ) -> anyhow::Result<()> {
    //     let mut it = file_infos
    //         .iter()
    //         .enumerate()
    //         .skip_while(|(id, fi)| absolute_offset < fi.offset_in_torrent);
    //     let (mut file_id, mut fi) = it.next().context("invalid offset")?;
    //     let mut file_offset = fi.offset_in_torrent - absolute_offset;
    //     while !buf.is_empty() {
    //         let to_read = (buf.len() as u64)
    //             .min(fi.len - file_offset)
    //             .try_into()
    //             .unwrap();
    //         if to_read == 0 {
    //             anyhow::bail!("bug, to_read = 0");
    //         }
    //         self.pread_exact(file_id, file_offset, &mut buf[..to_read])?;
    //         buf = &mut buf[to_read..];
    //         file_offset += to_read as u64;
    //         if file_offset == fi.len {
    //             (file_id, fi) = it.next().context("nowhere to read from")?;
    //             file_offset = 0;
    //         }
    //     }

    //     Ok(())
    // }

    fn pwrite_all_absolute(
        &self,
        absolute_offset: u64,
        mut buf: &[u8],
        file_infos: &FileInfos,
    ) -> anyhow::Result<()> {
        let mut it = file_infos
            .iter()
            .enumerate()
            .skip_while(|(_, fi)| absolute_offset < fi.offset_in_torrent);
        let (mut file_id, mut fi) = it.next().context("invalid offset")?;
        let mut file_offset = absolute_offset - fi.offset_in_torrent;
        while !buf.is_empty() {
            let to_read = (buf.len() as u64)
                .min(fi.len - file_offset)
                .try_into()
                .unwrap();
            if to_read == 0 {
                anyhow::bail!("bug, to_read = 0");
            }
            self.pwrite_all(file_id, file_offset, &buf[..to_read])?;
            buf = &buf[to_read..];
            file_offset += to_read as u64;
            if file_offset == fi.len {
                (file_id, fi) = it.next().context("nowhere to write")?;
                file_offset = 0;
            }
        }

        Ok(())
    }
}

impl<U: TorrentStorage + ?Sized> TorrentStorage for Box<U> {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        (**self).pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        (**self).pwrite_all(file_id, offset, buf)
    }

    fn remove_file(&self, file_id: usize, filename: &Path) -> anyhow::Result<()> {
        (**self).remove_file(file_id, filename)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        (**self).ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        (**self).take()
    }
}
