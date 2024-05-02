mod fs;
mod mmap;
mod opened_file;

pub use fs::{FilesystemStorage, FilesystemStorageFactory};
pub use mmap::{MmapFilesystemStorage, MmapFilesystemStorageFactory};
