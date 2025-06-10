mod fs;
mod mmap;
mod opened_file;
mod sparse;

pub use fs::{FilesystemStorage, FilesystemStorageFactory};
pub use mmap::{MmapFilesystemStorage, MmapFilesystemStorageFactory};
