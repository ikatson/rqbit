mod fs;
mod opened_file;
pub(crate) mod relocate;
mod sparse;

pub use fs::{FilesystemStorage, FilesystemStorageFactory};
pub use opened_file::OurFileExt;
