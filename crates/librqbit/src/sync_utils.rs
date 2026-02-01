use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use librqbit_core::torrent_metainfo::TorrentMetaV1Info;
use tracing::{info, warn};

pub fn remove_extra_files(
    info: &TorrentMetaV1Info<buffers::ByteBufOwned>,
    root_path: &Path,
) -> anyhow::Result<()> {
    if !root_path.exists() {
        return Ok(());
    }

    let mut expected_files: HashSet<PathBuf> = HashSet::new();

    // Populate expected files
    if let Some(files) = &info.files {
        for file in files {
            // file.path is Vec<ByteBuf>
            let mut path = PathBuf::new();
            for component in &file.path {
                path.push(&*bytes_to_osstr(&component.0));
            }
            expected_files.insert(path);
        }
    } else if let Some(name) = &info.name {
        // Single file mode
        let name_str = String::from_utf8_lossy(&name.0);
        expected_files.insert(PathBuf::from(name_str.as_ref()));
    }

    // Iterate and delete
    // We need to be careful. Single file torrent: root_path IS the file (usually? or parent?).
    // Usually librqbit downloads to `root_path / name`.
    // If output_folder is provided, that's where we look.
    
    // For multifile: `root_path / directory_name / ...` 
    // Wait, librqbit logic:
    // If multifile: `download_dir / torrent_name / ...`
    // If single file: `download_dir / torrent_name.ext`
    
    // We assume `root_path` PASSED to this function is the directory containing the torrent content.
    // E.g. for multifile, it's `.../Torrents/MyTorrent/`.
    
    // Let's walk the directory.
    for entry in walkdir::WalkDir::new(root_path)
        .min_depth(1)
        .contents_first(true) // Visit children before parents (good for deleting empty dirs)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path == root_path {
            continue;
        }

        let relative = match path.strip_prefix(root_path) {
            Ok(p) => p,
            Err(_) => continue,
        };

        if entry.file_type().is_dir() {
            // If it's a directory, check if it contains any expected files?
            // Or simpler: if we are post-order, if it's empty, delete it.
            // But we should only delete if it wasn't expected? Use expected_files.
            // Directories are not explicitly in `expected_files` usually, only files are.
            
            // Check if this directory is a prefix of any expected file.
            // This is slow if many files.
            // Optimization: `expected_files` contains full relative paths.
            // If `relative` is not a parent of any `expected_files` item, we can remove it?
            
            // Simpler: Just try to remove empty directories.
            // If it contains a kept file, `remove_dir` will fail (not recursive).
            if std::fs::remove_dir(path).is_ok() {
               info!("Removed empty directory: {:?}", relative);
            }
        } else {
             // It's a file.
             if !expected_files.contains(relative) {
                 info!("Removing extra file: {:?}", relative);
                 if let Err(e) = std::fs::remove_file(path) {
                     warn!("Failed to remove file {:?}: {:?}", path, e);
                 }
             }
        }
    }

    Ok(())
}

#[cfg(windows)]
fn bytes_to_osstr(b: &[u8]) -> std::borrow::Cow<'_, OsStr> {
    // This is a simplification. Real world torrents might have encoding mess.
    // We assume UTF-8 for valid filenames here since we are in Rust world.
    // If it fails, we fall back to lossy.
    use std::ffi::OsString;
    let s = String::from_utf8_lossy(b).into_owned();
    Cow::Owned(OsString::from(s))
}

#[cfg(unix)]
fn bytes_to_osstr(b: &[u8]) -> std::borrow::Cow<'_, OsStr> {
    use std::os::unix::ffi::OsStrExt;
    std::borrow::Cow::Borrowed(OsStr::from_bytes(b))
}

#[cfg(not(any(unix, windows)))]
fn bytes_to_osstr(b: &[u8]) -> std::borrow::Cow<'_, OsStr> {
    // Fallback for other OSes
    use std::ffi::OsString;
     let s = String::from_utf8_lossy(b).into_owned();
    Cow::Owned(OsString::from(s))
}
