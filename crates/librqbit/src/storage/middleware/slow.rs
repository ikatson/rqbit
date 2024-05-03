/*
A storage middleware that slows down the underlying storage.
*/

use std::{
    fs::File,
    io::{BufRead, BufReader, Lines},
    time::Duration,
};

use parking_lot::Mutex;

use crate::storage::{StorageFactory, StorageFactoryExt, TorrentStorage};

#[derive(Clone)]
pub struct SlowStorageFactory<U> {
    underlying_factory: U,
}

impl<U: StorageFactory> SlowStorageFactory<U> {
    pub fn new(underlying: U) -> Self {
        Self {
            underlying_factory: underlying,
        }
    }
}

impl<U: StorageFactory + Clone> StorageFactory for SlowStorageFactory<U> {
    type Storage = SlowStorage<U::Storage>;

    fn init_storage(&self, info: &crate::ManagedTorrentInfo) -> anyhow::Result<Self::Storage> {
        Ok(SlowStorage {
            underlying: self.underlying_factory.init_storage(info)?,
            pwrite_all_bufread: Mutex::new(
                BufReader::new(
                    File::open("/Users/igor/Downloads/rqbit-log-slow-disk.log-pwrite_all").unwrap(),
                )
                .lines(),
            ),
            pread_exact_bufread: Mutex::new(
                BufReader::new(
                    File::open("/Users/igor/Downloads/rqbit-log-slow-disk.log-pread_exact")
                        .unwrap(),
                )
                .lines(),
            ),
        })
    }

    fn is_type_id(&self, type_id: std::any::TypeId) -> bool {
        self.underlying_factory.is_type_id(type_id)
    }

    fn clone_box(&self) -> crate::storage::BoxStorageFactory {
        self.clone().boxed()
    }
}

pub struct SlowStorage<U> {
    underlying: U,
    pwrite_all_bufread: Mutex<Lines<BufReader<File>>>,
    pread_exact_bufread: Mutex<Lines<BufReader<File>>>,
}

fn sleep_from_reader(r: &Mutex<Lines<BufReader<File>>>) {
    let mut g = r.lock();
    let micros: u64 = g.next().unwrap().unwrap().parse().unwrap();
    let sl = Duration::from_micros(micros);
    std::thread::sleep(sl)
}

impl<U: TorrentStorage> TorrentStorage for SlowStorage<U> {
    fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
        sleep_from_reader(&self.pread_exact_bufread);
        self.underlying.pread_exact(file_id, offset, buf)
    }

    fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
        sleep_from_reader(&self.pwrite_all_bufread);
        self.underlying.pwrite_all(file_id, offset, buf)
    }

    fn remove_file(&self, file_id: usize, filename: &std::path::Path) -> anyhow::Result<()> {
        self.underlying.remove_file(file_id, filename)
    }

    fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
        self.underlying.ensure_file_length(file_id, length)
    }

    fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
        anyhow::bail!("not implemented")
    }
}
