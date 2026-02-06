use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{BufWriter, Read};
use std::path::{Path, PathBuf};

use anyhow::Context;
use bencode::{WithRawBytes, bencode_serialize_to_writer};
use buffers::ByteBufOwned;
use bytes::Bytes;
use librqbit_core::magnet::Magnet;
use librqbit_core::merkle::{MERKLE_BLOCK_SIZE, compute_merkle_root, hash_block};
use librqbit_core::torrent_metainfo::{
    TorrentMetaV1File, TorrentMetaV1Info, TorrentMetaV1Owned, TorrentVersion, V2FileEntry,
    V2FileTreeNode,
};
use librqbit_core::{Id20, Id32};
use sha1w::{ISha1, ISha256};

use crate::spawn_utils::BlockingSpawner;

#[derive(Debug, Clone, Default)]
pub struct CreateTorrentOptions<'a> {
    pub name: Option<&'a str>,
    pub trackers: Vec<String>,
    pub piece_length: Option<u32>,
    /// Torrent version to create. `None` defaults to V1Only for backward compat.
    pub version: Option<TorrentVersion>,
}

fn walk_dir_find_paths(dir: &Path, out: &mut Vec<Cow<'_, Path>>) -> anyhow::Result<()> {
    out.extend(
        walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.path().to_owned().into()),
    );
    Ok(())
}

/// Compute info hashes from the info dict.
/// Returns (SHA-1 hash, optional SHA-256 hash, raw bencoded info bytes).
fn compute_info_hash(
    t: &TorrentMetaV1Info<ByteBufOwned>,
) -> anyhow::Result<(Id20, Option<Id32>, Bytes)> {
    let mut writer = BufWriter::new(Vec::new());
    bencode_serialize_to_writer(t, &mut writer)?;
    let bytes: Bytes = writer
        .into_inner()
        .map_err(|_| anyhow::anyhow!("into_inner errored"))?
        .into();

    let info_hash = Id20::new({
        let mut h = sha1w::Sha1::new();
        h.update(&bytes);
        h.finish()
    });

    let info_hash_v2 = if t.meta_version == Some(2) && t.file_tree.is_some() {
        Some(Id32::new({
            let mut h = sha1w::Sha256::new();
            h.update(&bytes);
            h.finish()
        }))
    } else {
        None
    };

    Ok((info_hash, info_hash_v2, bytes))
}

fn choose_piece_length(_input_files: &[Cow<'_, Path>]) -> u32 {
    // TODO: make this smarter or smth
    2 * 1024 * 1024
}

#[cfg(unix)]
fn osstr_to_bytes(o: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    o.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn osstr_to_bytes(o: &OsStr) -> Vec<u8> {
    o.to_string_lossy().as_bytes().to_vec()
}

/// Sort files by raw byte order of their path components relative to `base`.
fn sort_files_by_raw_byte_path(files: &mut [Cow<'_, Path>], base: &Path) {
    files.sort_by(|a, b| {
        let a_rel = a.strip_prefix(base).unwrap_or(a.as_ref());
        let b_rel = b.strip_prefix(base).unwrap_or(b.as_ref());
        let a_components: Vec<Vec<u8>> = a_rel
            .components()
            .map(|c| osstr_to_bytes(c.as_os_str()))
            .collect();
        let b_components: Vec<Vec<u8>> = b_rel
            .components()
            .map(|c| osstr_to_bytes(c.as_os_str()))
            .collect();
        a_components.cmp(&b_components)
    });
}

/// Insert a file entry into a v2 file tree at the given path components.
fn insert_into_file_tree(
    tree: &mut BTreeMap<ByteBufOwned, V2FileTreeNode<ByteBufOwned>>,
    path_components: &[Vec<u8>],
    entry: V2FileEntry,
) {
    assert!(!path_components.is_empty());
    if path_components.len() == 1 {
        tree.insert(
            ByteBufOwned::from(&path_components[0][..]),
            V2FileTreeNode::File(entry),
        );
    } else {
        let key = ByteBufOwned::from(&path_components[0][..]);
        let node = tree
            .entry(key)
            .or_insert_with(|| V2FileTreeNode::Directory(BTreeMap::new()));
        match node {
            V2FileTreeNode::Directory(children) => {
                insert_into_file_tree(children, &path_components[1..], entry);
            }
            V2FileTreeNode::File(_) => {
                panic!("path conflict: expected directory but found file");
            }
        }
    }
}

struct CreateTorrentRawResult {
    info: TorrentMetaV1Info<ByteBufOwned>,
    output_folder: PathBuf,
    piece_layers: Option<BTreeMap<Id32, ByteBufOwned>>,
}

async fn create_torrent_raw<'a>(
    path: &'a Path,
    options: CreateTorrentOptions<'a>,
    spawner: &BlockingSpawner,
) -> anyhow::Result<CreateTorrentRawResult> {
    path.try_exists()
        .with_context(|| format!("path {path:?} doesn't exist"))?;
    let basename = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("cannot determine basename of {path:?}"))?;
    let is_dir = path.is_dir();
    let name: ByteBufOwned = match options.name {
        Some(name) => name.as_bytes().into(),
        None => osstr_to_bytes(basename).into(),
    };
    let output_folder: PathBuf;

    let version = options.version.unwrap_or(TorrentVersion::V1Only);
    let do_v1 = matches!(version, TorrentVersion::V1Only | TorrentVersion::Hybrid);
    let do_v2 = matches!(version, TorrentVersion::V2Only | TorrentVersion::Hybrid);
    let is_hybrid = matches!(version, TorrentVersion::Hybrid);

    let mut input_files: Vec<Cow<'a, Path>> = Default::default();
    if is_dir {
        output_folder = path.to_owned();
        walk_dir_find_paths(path, &mut input_files)
            .with_context(|| format!("error walking {path:?}"))?;
    } else {
        output_folder = path
            .canonicalize()?
            .parent()
            .context("single file has no parent")?
            .to_path_buf();
        input_files.push(Cow::Borrowed(path));
    }

    // BEP 52: sort files by raw byte order of path components.
    if do_v2 {
        let sort_base = if is_dir { path } else { &output_folder };
        sort_files_by_raw_byte_path(&mut input_files, sort_base);
    }

    let piece_length = options
        .piece_length
        .unwrap_or_else(|| choose_piece_length(&input_files));

    // Validate piece_length for v2/hybrid.
    if do_v2 {
        anyhow::ensure!(
            piece_length.is_power_of_two() && piece_length >= MERKLE_BLOCK_SIZE,
            "v2 piece_length must be a power of two and >= {MERKLE_BLOCK_SIZE}, got {piece_length}"
        );
    }

    let _permit = spawner.semaphore().acquire_owned().await?;

    // Use MERKLE_BLOCK_SIZE as read buffer for v2 (feeds both pipelines).
    let read_size = if do_v2 {
        MERKLE_BLOCK_SIZE as usize
    } else {
        8192
    };
    let mut read_buf = vec![0u8; read_size];

    // v1 state
    let mut v1_remaining_piece_length = piece_length;
    let mut v1_piece_checksum = sha1w::Sha1::new();
    let mut v1_piece_hashes = Vec::<u8>::new();
    let mut v1_output_files: Vec<TorrentMetaV1File<ByteBufOwned>> = Vec::new();
    let mut v1_any_data_hashed = false;

    // v2 state
    let mut v2_file_tree: BTreeMap<ByteBufOwned, V2FileTreeNode<ByteBufOwned>> = BTreeMap::new();
    let mut v2_piece_layers: BTreeMap<Id32, ByteBufOwned> = BTreeMap::new();
    let blocks_per_piece = piece_length / MERKLE_BLOCK_SIZE;

    let total_files = input_files.len();
    for (index, file) in input_files.iter().enumerate() {
        let filename = &**file;
        let is_last_file = index + 1 == total_files;
        let mut file_length: u64 = 0;
        let mut v2_block_hashes: Vec<Id32> = Vec::new();

        let mut fd = std::io::BufReader::new(
            std::fs::File::open(file).with_context(|| format!("error opening {filename:?}"))?,
        );

        loop {
            let max_bytes_to_read = if do_v1 {
                // For v1 pipeline, respect piece boundaries.
                (v1_remaining_piece_length as usize).min(read_size)
            } else {
                read_size
            };

            let size = spawner
                .block_in_place(|| fd.read(&mut read_buf[..max_bytes_to_read]))
                .with_context(|| format!("error reading {filename:?}"))?;

            if size == 0 {
                // EOF for this file.
                break;
            }

            file_length += size as u64;

            // v1 pipeline: feed data to SHA-1 piece hasher.
            if do_v1 {
                v1_piece_checksum.update(&read_buf[..size]);
                v1_any_data_hashed = true;
                #[allow(clippy::cast_possible_truncation)] // size bounded by piece_length (u32)
                {
                    v1_remaining_piece_length -= size as u32;
                }
                if v1_remaining_piece_length == 0 {
                    v1_remaining_piece_length = piece_length;
                    v1_piece_hashes.extend_from_slice(&v1_piece_checksum.finish());
                    v1_piece_checksum = sha1w::Sha1::new();
                    v1_any_data_hashed = false;
                }
            }

            // v2 pipeline: hash each MERKLE_BLOCK_SIZE block.
            if do_v2 {
                v2_block_hashes.push(hash_block(&read_buf[..size]));
            }
        }

        // End of file: build file entries.
        // v2 file_tree paths are NOT prefixed by the torrent name.
        let path_components: Vec<Vec<u8>> = if is_dir {
            let rel_path = filename
                .strip_prefix(path)
                .context("internal error, can't strip prefix")?;
            rel_path
                .components()
                .map(|c| osstr_to_bytes(c.as_os_str()))
                .collect()
        } else {
            // Single file: use just the file name.
            vec![osstr_to_bytes(basename)]
        };

        // v1 output file entry (for v1-only and hybrid).
        if do_v1 {
            v1_output_files.push(TorrentMetaV1File {
                length: file_length,
                path: path_components
                    .iter()
                    .map(|c| ByteBufOwned::from(&c[..]))
                    .collect(),
                attr: None,
                sha1: None,
                symlink_path: None,
            });
        }

        // v2 file tree entry.
        if do_v2 {
            let entry = if file_length == 0 {
                V2FileEntry {
                    length: 0,
                    pieces_root: None,
                }
            } else {
                let merkle = compute_merkle_root(&v2_block_hashes, blocks_per_piece);
                let pieces_root = merkle.root;

                // Multi-piece files need piece_layers entry.
                if merkle.piece_hashes.len() > 1 {
                    let mut layer_bytes = Vec::with_capacity(merkle.piece_hashes.len() * 32);
                    for ph in &merkle.piece_hashes {
                        layer_bytes.extend_from_slice(&ph.0);
                    }
                    v2_piece_layers.insert(pieces_root, ByteBufOwned::from(&layer_bytes[..]));
                }

                V2FileEntry {
                    length: file_length,
                    pieces_root: Some(pieces_root),
                }
            };
            insert_into_file_tree(&mut v2_file_tree, &path_components, entry);
        }

        // Hybrid: pad v1 to piece boundary after each file.
        if is_hybrid && !is_last_file && file_length > 0 {
            let remainder = file_length % piece_length as u64;
            if remainder != 0 {
                let pad_size = piece_length as u64 - remainder;

                // Feed zeros to v1 SHA-1 hasher for the remaining piece bytes.
                let mut pad_remaining = pad_size;
                let zero_buf = vec![0u8; read_size];
                #[allow(clippy::cast_possible_truncation)] // values bounded by piece_length (u32)
                while pad_remaining > 0 {
                    let chunk = (pad_remaining as usize).min(read_size);
                    let to_feed = chunk.min(v1_remaining_piece_length as usize);
                    v1_piece_checksum.update(&zero_buf[..to_feed]);
                    v1_any_data_hashed = true;
                    v1_remaining_piece_length -= to_feed as u32;
                    pad_remaining -= to_feed as u64;
                    if v1_remaining_piece_length == 0 {
                        v1_remaining_piece_length = piece_length;
                        v1_piece_hashes.extend_from_slice(&v1_piece_checksum.finish());
                        v1_piece_checksum = sha1w::Sha1::new();
                        v1_any_data_hashed = false;
                    }
                }

                // Insert padding file entry in v1 file list.
                v1_output_files.push(TorrentMetaV1File {
                    length: pad_size,
                    path: vec![
                        ByteBufOwned::from(b".pad".as_slice()),
                        ByteBufOwned::from(pad_size.to_string().as_bytes()),
                    ],
                    attr: Some(ByteBufOwned::from(b"p".as_slice())),
                    sha1: None,
                    symlink_path: None,
                });
            }
        }
    }

    // Finalize v1: flush last partial piece.
    if do_v1 && v1_any_data_hashed {
        v1_piece_hashes.extend_from_slice(&v1_piece_checksum.finish());
    }

    // For v1 single-file mode (v1-only or hybrid): no `files` list, use `length` instead.
    let single_file_mode = !is_dir && do_v1;

    // Determine the total length for single-file mode.
    let single_file_length = if single_file_mode {
        v1_output_files.first().map(|f| f.length)
    } else {
        None
    };

    Ok(CreateTorrentRawResult {
        info: TorrentMetaV1Info {
            name: Some(name),
            pieces: if do_v1 {
                Some(v1_piece_hashes.into())
            } else {
                None
            },
            piece_length,
            length: single_file_length,
            md5sum: None,
            files: if single_file_mode || !do_v1 {
                None
            } else {
                Some(v1_output_files)
            },
            attr: None,
            sha1: None,
            symlink_path: None,
            private: false,
            meta_version: if do_v2 { Some(2) } else { None },
            file_tree: if do_v2 { Some(v2_file_tree) } else { None },
        },
        output_folder,
        piece_layers: if do_v2 && !v2_piece_layers.is_empty() {
            Some(v2_piece_layers)
        } else if do_v2 {
            // v2 torrents always have piece_layers field (may be empty map).
            Some(BTreeMap::new())
        } else {
            None
        },
    })
}

#[derive(Debug)]
pub struct CreateTorrentResult {
    pub meta: TorrentMetaV1Owned,
    pub output_folder: PathBuf,
}

impl CreateTorrentResult {
    pub fn as_info(&self) -> &TorrentMetaV1Owned {
        &self.meta
    }

    pub fn info_hash(&self) -> Id20 {
        self.meta.info_hash
    }

    pub fn info_hash_v2(&self) -> Option<Id32> {
        self.meta.info_hash_v2
    }

    pub fn as_magnet(&self) -> Magnet {
        let trackers = self
            .meta
            .iter_announce()
            .map(|i| std::str::from_utf8(i.as_ref()).unwrap().to_owned())
            .collect();
        let has_v1 = self
            .meta
            .info
            .data
            .pieces
            .as_ref()
            .is_some_and(|p| !p.as_ref().is_empty());
        let id20 = if has_v1 { Some(self.info_hash()) } else { None };
        Magnet::new(id20, self.info_hash_v2(), trackers, None)
    }

    pub fn as_bytes(&self) -> anyhow::Result<Bytes> {
        let mut b = Vec::new();
        bencode_serialize_to_writer(&self.meta, &mut b).context("error serializing torrent")?;
        Ok(b.into())
    }
}

pub async fn create_torrent<'a>(
    path: &'a Path,
    options: CreateTorrentOptions<'a>,
    spawner: &BlockingSpawner,
) -> anyhow::Result<CreateTorrentResult> {
    let trackers = options
        .trackers
        .iter()
        .map(|t| ByteBufOwned::from(t.as_bytes()))
        .collect();
    let res = create_torrent_raw(path, options, spawner).await?;
    let (info_hash, info_hash_v2, bytes) =
        compute_info_hash(&res.info).context("error computing info hash")?;

    let piece_layers = res.piece_layers;

    Ok(CreateTorrentResult {
        meta: TorrentMetaV1Owned {
            announce: None,
            announce_list: vec![trackers],
            info: WithRawBytes {
                data: res.info,
                raw_bytes: ByteBufOwned(bytes),
            },
            comment: None,
            created_by: None,
            encoding: Some(b"utf-8"[..].into()),
            publisher: None,
            publisher_url: None,
            creation_date: None,
            info_hash,
            info_hash_v2,
            piece_layers,
        },
        output_folder: res.output_folder,
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::PathBuf;
    use std::sync::Arc;

    use anyhow::Context;
    use buffers::ByteBufOwned;
    use bytes::Bytes;
    use clone_to_owned::CloneToOwned;
    use librqbit_core::torrent_metainfo::{TorrentVersion, collect_v2_files, torrent_from_bytes};
    use parking_lot::RwLock;

    use crate::file_ops::FileOps;
    use crate::storage::TorrentStorage;
    use crate::tests::test_util;
    use crate::type_aliases::FileInfos;
    use crate::{create_torrent, spawn_utils::BlockingSpawner, torrent_state::TorrentMetadata};

    use super::CreateTorrentOptions;

    struct TestFsStorage {
        root: PathBuf,
        file_infos: FileInfos,
    }

    impl TorrentStorage for TestFsStorage {
        fn pread_exact(&self, file_id: usize, offset: u64, buf: &mut [u8]) -> anyhow::Result<()> {
            let fi = self.file_infos.get(file_id).context("no such file")?;
            let path = self.root.join(&fi.relative_filename);
            let mut f = std::fs::OpenOptions::new().read(true).open(&path)?;
            f.seek(SeekFrom::Start(offset))?;
            f.read_exact(buf)?;
            Ok(())
        }

        fn pwrite_all(&self, file_id: usize, offset: u64, buf: &[u8]) -> anyhow::Result<()> {
            let fi = self.file_infos.get(file_id).context("no such file")?;
            let path = self.root.join(&fi.relative_filename);
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)?;
            f.seek(SeekFrom::Start(offset))?;
            f.write_all(buf)?;
            Ok(())
        }

        fn remove_file(&self, _file_id: usize, _filename: &std::path::Path) -> anyhow::Result<()> {
            Ok(())
        }

        fn remove_directory_if_empty(&self, _path: &std::path::Path) -> anyhow::Result<()> {
            Ok(())
        }

        fn ensure_file_length(&self, file_id: usize, length: u64) -> anyhow::Result<()> {
            let fi = self.file_infos.get(file_id).context("no such file")?;
            let path = self.root.join(&fi.relative_filename);
            let f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            f.set_len(length)?;
            Ok(())
        }

        fn take(&self) -> anyhow::Result<Box<dyn TorrentStorage>> {
            Ok(Box::new(Self {
                root: self.root.clone(),
                file_infos: self.file_infos.clone(),
            }))
        }

        fn init(
            &mut self,
            _shared: &crate::torrent_state::ManagedTorrentShared,
            _metadata: &crate::torrent_state::TorrentMetadata,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_create_torrent() {
        let dir = test_util::create_default_random_dir_with_torrents(
            3,
            1000 * 1000,
            Some("rqbit_test_create_torrent"),
        );
        let torrent = create_torrent(dir.path(), Default::default(), &BlockingSpawner::new(1))
            .await
            .unwrap();

        let bytes = torrent.as_bytes().unwrap();

        let deserialized = torrent_from_bytes(&bytes).unwrap();
        assert_eq!(torrent.info_hash(), deserialized.info_hash);
    }

    #[tokio::test]
    async fn test_create_v2_torrent() {
        let dir = test_util::create_default_random_dir_with_torrents(
            3,
            1000 * 1000,
            Some("rqbit_test_create_v2_torrent"),
        );
        let torrent = create_torrent(
            dir.path(),
            CreateTorrentOptions {
                version: Some(TorrentVersion::V2Only),
                piece_length: Some(65536),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await
        .unwrap();

        assert!(torrent.info_hash_v2().is_some());

        let bytes = torrent.as_bytes().unwrap();
        let deserialized = torrent_from_bytes(&bytes).unwrap();

        assert_eq!(
            deserialized.version(),
            Some(TorrentVersion::V2Only),
            "should be v2-only"
        );
        assert_eq!(deserialized.info_hash_v2, torrent.info_hash_v2());
        assert!(
            deserialized.info.data.pieces.is_none(),
            "v2-only should have no pieces"
        );
        assert!(
            deserialized.info.data.files.is_none(),
            "v2-only should have no v1 files"
        );
        assert!(
            deserialized.info.data.length.is_none(),
            "v2-only should have no v1 length"
        );

        let magnet = torrent.as_magnet();
        assert!(
            magnet.as_id20().is_none(),
            "v2-only magnet should omit id20"
        );
        assert!(
            magnet.as_id32().is_some(),
            "v2-only magnet should include id32"
        );

        let file_tree = deserialized.info.data.file_tree.as_ref().unwrap();
        let files = collect_v2_files(file_tree);
        assert_eq!(files.len(), 3, "should have 3 files");

        assert!(
            deserialized.piece_layers.is_some(),
            "v2 should have piece_layers"
        );

        deserialized
            .validate_v2_piece_layers()
            .expect("piece_layers validation should pass");
    }

    #[tokio::test]
    async fn test_create_hybrid_torrent() {
        let dir = test_util::create_default_random_dir_with_torrents(
            3,
            1000 * 1000,
            Some("rqbit_test_create_hybrid_torrent"),
        );
        let torrent = create_torrent(
            dir.path(),
            CreateTorrentOptions {
                version: Some(TorrentVersion::Hybrid),
                piece_length: Some(65536),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await
        .unwrap();

        assert!(torrent.info_hash_v2().is_some(), "should have v2 hash");

        let bytes = torrent.as_bytes().unwrap();
        let deserialized = torrent_from_bytes(&bytes).unwrap();

        assert_eq!(
            deserialized.version(),
            Some(TorrentVersion::Hybrid),
            "should be hybrid"
        );
        assert!(
            deserialized.info.data.pieces.is_some(),
            "hybrid should have v1 pieces"
        );
        assert!(
            deserialized.info.data.file_tree.is_some(),
            "hybrid should have file_tree"
        );

        deserialized
            .validate_v2_piece_layers()
            .expect("piece_layers validation should pass");

        // Check padding files exist in v1 file list.
        let v1_files = deserialized.info.data.files.as_ref().unwrap();
        let padding_files: Vec<_> = v1_files
            .iter()
            .filter(|f| f.attr.as_ref().is_some_and(|a| a.as_ref() == b"p"))
            .collect();
        // With 3 files of ~1MB each and 64KB pieces, each file likely doesn't end
        // on a piece boundary, so we expect padding files.
        assert!(
            !padding_files.is_empty(),
            "hybrid should have padding files"
        );

        // Real files (excluding padding) should match v2 file count.
        let real_v1_files = v1_files
            .iter()
            .filter(|f| f.attr.as_ref().is_none_or(|a| a.as_ref() != b"p"))
            .count();
        let file_tree = deserialized.info.data.file_tree.as_ref().unwrap();
        let v2_files = collect_v2_files(file_tree);
        assert_eq!(
            real_v1_files,
            v2_files.len(),
            "v1 real file count should match v2"
        );
    }

    #[tokio::test]
    async fn test_create_hybrid_padding_not_after_last_file() {
        let dir = tempfile::TempDir::with_prefix("rqbit_test_hybrid_pad_last").unwrap();
        let file_a = dir.path().join("a.bin");
        let file_b = dir.path().join("b.bin");
        test_util::create_new_file_with_random_content(&file_a, 100_000);
        test_util::create_new_file_with_random_content(&file_b, 120_000);

        let torrent = create_torrent(
            dir.path(),
            CreateTorrentOptions {
                version: Some(TorrentVersion::Hybrid),
                piece_length: Some(65536),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await
        .unwrap();

        let bytes = torrent.as_bytes().unwrap();
        let deserialized = torrent_from_bytes(&bytes).unwrap();
        let v1_files = deserialized.info.data.files.as_ref().unwrap();

        let padding_files: Vec<_> = v1_files
            .iter()
            .filter(|f| f.attr.as_ref().is_some_and(|a| a.as_ref() == b"p"))
            .collect();
        assert_eq!(
            padding_files.len(),
            1,
            "should insert padding only between files"
        );

        let last = v1_files.last().unwrap();
        assert!(
            last.attr.as_ref().is_none_or(|a| a.as_ref() != b"p"),
            "last v1 entry should be a real file, not padding"
        );
    }

    #[tokio::test]
    async fn test_hybrid_padding_ignored_for_v2_verification() {
        let dir = tempfile::TempDir::with_prefix("rqbit_test_hybrid_pad_v2").unwrap();
        let file_a = dir.path().join("a.bin");
        let file_b = dir.path().join("b.bin");
        test_util::create_new_file_with_random_content(&file_a, 100_000);
        test_util::create_new_file_with_random_content(&file_b, 120_000);

        let torrent = create_torrent(
            dir.path(),
            CreateTorrentOptions {
                version: Some(TorrentVersion::Hybrid),
                piece_length: Some(65536),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await
        .unwrap();

        let bytes = torrent.as_bytes().unwrap();
        let parsed = torrent_from_bytes(&bytes).unwrap();

        let v1_files = parsed.info.data.files.as_ref().unwrap();
        let padding_files: Vec<_> = v1_files
            .iter()
            .filter(|f| f.attr.as_ref().is_some_and(|a| a.as_ref() == b"p"))
            .collect();
        assert!(
            !padding_files.is_empty(),
            "expected padding files for hybrid torrent"
        );

        let info_owned = parsed.info.data.clone_to_owned(Some(&bytes));
        let validated = info_owned.clone().validate().unwrap();
        let v2_lengths = validated
            .v2_lengths()
            .expect("hybrid should have v2_lengths");

        let v2_files = collect_v2_files(parsed.info.data.file_tree.as_ref().unwrap());
        assert_eq!(v2_files.len(), 2, "expected two v2 files");

        let piece_index = validated
            .lengths()
            .validate_piece_index(v2_lengths.files()[1].first_piece_index)
            .unwrap();

        let piece_layers = parsed
            .piece_layers
            .as_ref()
            .expect("hybrid should have piece_layers");
        let piece_layers_bytes = piece_layers
            .iter()
            .map(|(k, v)| (*k, Bytes::copy_from_slice(v.as_ref())))
            .collect::<std::collections::BTreeMap<_, _>>();

        let info_bytes = Bytes::copy_from_slice(parsed.info.raw_bytes.as_ref());
        let metadata = TorrentMetadata::new(
            validated.clone(),
            bytes.clone(),
            info_bytes,
            Some(piece_layers_bytes),
        )
        .unwrap();
        let file_infos = metadata.file_infos.clone();
        let storage = TestFsStorage {
            root: dir.path().to_path_buf(),
            file_infos: file_infos.clone(),
        };

        let fo = FileOps::new(
            &validated,
            &storage,
            &file_infos,
            metadata.piece_layers.clone(),
        );
        assert!(
            fo.check_piece(piece_index).unwrap(),
            "v2 verification should ignore v1 padding files"
        );
    }

    #[tokio::test]
    async fn test_hybrid_rejects_mismatched_hashes() {
        let dir = test_util::create_default_random_dir_with_torrents(
            2,
            256 * 1024,
            Some("rqbit_test_hybrid_mismatch"),
        );
        let torrent = create_torrent(
            dir.path(),
            CreateTorrentOptions {
                version: Some(TorrentVersion::Hybrid),
                piece_length: Some(65536),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await
        .unwrap();

        let bytes = torrent.as_bytes().unwrap();
        let parsed = torrent_from_bytes(&bytes).unwrap();
        let info_owned = parsed.info.data.clone_to_owned(Some(&bytes));
        let validated = info_owned.clone().validate().unwrap();

        let piece_layers = parsed
            .piece_layers
            .as_ref()
            .expect("hybrid should have piece_layers");
        let piece_layers_bytes = piece_layers
            .iter()
            .map(|(k, v)| (*k, Bytes::copy_from_slice(v.as_ref())))
            .collect::<std::collections::BTreeMap<_, _>>();

        let info_bytes = Bytes::copy_from_slice(parsed.info.raw_bytes.as_ref());
        let metadata = TorrentMetadata::new(
            validated.clone(),
            bytes.clone(),
            info_bytes,
            Some(piece_layers_bytes.clone()),
        )
        .unwrap();
        let file_infos = metadata.file_infos.clone();
        let storage = TestFsStorage {
            root: dir.path().to_path_buf(),
            file_infos: file_infos.clone(),
        };

        let v2_lengths = validated
            .v2_lengths()
            .expect("hybrid should have v2_lengths");
        let v2_files = collect_v2_files(parsed.info.data.file_tree.as_ref().unwrap());
        let (v2_file_idx, v2_file_info) = v2_lengths
            .files()
            .iter()
            .enumerate()
            .find(|(_, f)| f.num_pieces > 1)
            .expect("need a multi-piece file for v2 mismatch test");
        let pieces_root = v2_files
            .get(v2_file_idx)
            .and_then(|f| f.entry.pieces_root.as_ref())
            .expect("multi-piece file should have pieces_root");
        let piece_index = validated
            .lengths()
            .validate_piece_index(v2_file_info.first_piece_index)
            .unwrap();

        let piece_layers_arc = Arc::new(RwLock::new(Some(piece_layers_bytes.clone())));
        let fo = FileOps::new(&validated, &storage, &file_infos, piece_layers_arc.clone());
        assert!(
            fo.check_piece(piece_index).unwrap(),
            "baseline should verify"
        );

        // Tamper v1 piece hashes.
        let mut info_v1_tampered = info_owned.clone();
        let mut pieces = info_v1_tampered.pieces.as_ref().unwrap().as_ref().to_vec();
        pieces[0] ^= 0x01;
        info_v1_tampered.pieces = Some(ByteBufOwned(Bytes::from(pieces)));
        let validated_v1 = info_v1_tampered.validate().unwrap();
        let fo_v1 = FileOps::new(
            &validated_v1,
            &storage,
            &file_infos,
            piece_layers_arc.clone(),
        );
        assert!(
            !fo_v1.check_piece(piece_index).unwrap(),
            "hybrid should reject v1 mismatch"
        );

        // Tamper v2 piece layers.
        let mut piece_layers_bad = piece_layers_bytes.clone();
        if let Some(v) = piece_layers_bad.get_mut(pieces_root) {
            let mut data = v.to_vec();
            data[0] ^= 0x01;
            *v = Bytes::from(data);
        } else {
            panic!("expected piece_layers entry for multi-piece file");
        }
        let piece_layers_bad_arc = Arc::new(RwLock::new(Some(piece_layers_bad)));
        let fo_v2 = FileOps::new(&validated, &storage, &file_infos, piece_layers_bad_arc);
        assert!(
            !fo_v2.check_piece(piece_index).unwrap(),
            "hybrid should reject v2 mismatch"
        );
    }

    #[tokio::test]
    async fn test_v2_magnet_hash_response_flow_simulated() {
        let dir = test_util::create_default_random_dir_with_torrents(
            1,
            256 * 1024,
            Some("rqbit_test_v2_magnet_flow"),
        );
        let torrent = create_torrent(
            dir.path(),
            CreateTorrentOptions {
                version: Some(TorrentVersion::V2Only),
                piece_length: Some(65536),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await
        .unwrap();

        let bytes = torrent.as_bytes().unwrap();
        let parsed = torrent_from_bytes(&bytes).unwrap();
        let info_owned = parsed.info.data.clone_to_owned(Some(&bytes));
        let validated = info_owned.clone().validate().unwrap();

        let piece_layers = parsed
            .piece_layers
            .as_ref()
            .expect("v2-only should have piece_layers");
        let piece_layers_bytes = piece_layers
            .iter()
            .map(|(k, v)| (*k, Bytes::copy_from_slice(v.as_ref())))
            .collect::<std::collections::BTreeMap<_, _>>();

        let info_bytes = Bytes::copy_from_slice(parsed.info.raw_bytes.as_ref());
        let metadata_no_layers =
            TorrentMetadata::new(validated.clone(), bytes.clone(), info_bytes, None).unwrap();
        let file_infos = metadata_no_layers.file_infos.clone();
        let storage = TestFsStorage {
            root: dir.path().to_path_buf(),
            file_infos: file_infos.clone(),
        };

        let v2_lengths = validated
            .v2_lengths()
            .expect("v2-only should have v2_lengths");
        let piece_index = validated
            .lengths()
            .validate_piece_index(v2_lengths.files()[0].first_piece_index)
            .unwrap();

        // Simulate magnet flow before hash response: piece_layers missing.
        let piece_layers_missing = Arc::new(RwLock::new(None));
        let fo_missing = FileOps::new(&validated, &storage, &file_infos, piece_layers_missing);
        assert!(
            !fo_missing.check_piece(piece_index).unwrap(),
            "missing piece_layers should be treated as not ready"
        );

        // Simulate hash response by providing piece_layers.
        let piece_layers_ready = Arc::new(RwLock::new(Some(piece_layers_bytes)));
        let fo_ready = FileOps::new(&validated, &storage, &file_infos, piece_layers_ready);
        assert!(
            fo_ready.check_piece(piece_index).unwrap(),
            "piece should verify after piece_layers are available"
        );
    }

    #[tokio::test]
    async fn test_create_v2_single_file() {
        // Small file: < piece_length, so no piece_layers entry.
        {
            let dir = tempfile::TempDir::with_prefix("rqbit_test_v2_single_small").unwrap();
            let file_path = dir.path().join("small.bin");
            test_util::create_new_file_with_random_content(&file_path, 1000);

            let torrent = create_torrent(
                &file_path,
                CreateTorrentOptions {
                    version: Some(TorrentVersion::V2Only),
                    piece_length: Some(65536),
                    ..Default::default()
                },
                &BlockingSpawner::new(1),
            )
            .await
            .unwrap();

            let bytes = torrent.as_bytes().unwrap();
            let deserialized = torrent_from_bytes(&bytes).unwrap();

            assert_eq!(deserialized.version(), Some(TorrentVersion::V2Only));

            let file_tree = deserialized.info.data.file_tree.as_ref().unwrap();
            let files = collect_v2_files(file_tree);
            assert_eq!(files.len(), 1);
            assert!(files[0].entry.pieces_root.is_some());

            // Small file: no piece_layers entry for it.
            let piece_layers = deserialized.piece_layers.as_ref().unwrap();
            assert!(
                piece_layers.is_empty(),
                "small single file should have no piece_layers entries"
            );
        }

        // Large file: > piece_length, so piece_layers entry exists.
        {
            let dir = tempfile::TempDir::with_prefix("rqbit_test_v2_single_large").unwrap();
            let file_path = dir.path().join("large.bin");
            test_util::create_new_file_with_random_content(&file_path, 200_000);

            let torrent = create_torrent(
                &file_path,
                CreateTorrentOptions {
                    version: Some(TorrentVersion::V2Only),
                    piece_length: Some(65536),
                    ..Default::default()
                },
                &BlockingSpawner::new(1),
            )
            .await
            .unwrap();

            let bytes = torrent.as_bytes().unwrap();
            let deserialized = torrent_from_bytes(&bytes).unwrap();

            assert_eq!(deserialized.version(), Some(TorrentVersion::V2Only));

            let piece_layers = deserialized.piece_layers.as_ref().unwrap();
            assert!(
                !piece_layers.is_empty(),
                "large single file should have piece_layers entry"
            );

            deserialized
                .validate_v2_piece_layers()
                .expect("piece_layers validation should pass");
        }
    }

    #[tokio::test]
    async fn test_create_hybrid_single_file_uses_v1_single_file_mode() {
        let dir = tempfile::TempDir::with_prefix("rqbit_test_hybrid_single_file").unwrap();
        let file_path = dir.path().join("single.bin");
        test_util::create_new_file_with_random_content(&file_path, 100_000);

        let torrent = create_torrent(
            &file_path,
            CreateTorrentOptions {
                version: Some(TorrentVersion::Hybrid),
                piece_length: Some(65536),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await
        .unwrap();

        let bytes = torrent.as_bytes().unwrap();
        let deserialized = torrent_from_bytes(&bytes).unwrap();

        assert_eq!(
            deserialized.version(),
            Some(TorrentVersion::Hybrid),
            "should be hybrid"
        );
        assert!(
            deserialized.info.data.length.is_some(),
            "hybrid single-file should use v1 length field"
        );
        assert!(
            deserialized.info.data.files.is_none(),
            "hybrid single-file should omit v1 files list"
        );
        assert!(
            deserialized.info.data.pieces.is_some(),
            "hybrid should still include v1 pieces"
        );
        assert!(
            deserialized.info.data.file_tree.is_some(),
            "hybrid should include v2 file_tree"
        );
    }

    #[tokio::test]
    async fn test_create_v2_empty_file() {
        let dir = tempfile::TempDir::with_prefix("rqbit_test_v2_empty_file").unwrap();
        // Create one empty file and one normal file.
        std::fs::write(dir.path().join("empty.bin"), b"").unwrap();
        test_util::create_new_file_with_random_content(&dir.path().join("normal.bin"), 100_000);

        let torrent = create_torrent(
            dir.path(),
            CreateTorrentOptions {
                version: Some(TorrentVersion::V2Only),
                piece_length: Some(65536),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await
        .unwrap();

        let bytes = torrent.as_bytes().unwrap();
        let deserialized = torrent_from_bytes(&bytes).unwrap();

        assert_eq!(deserialized.version(), Some(TorrentVersion::V2Only));

        let file_tree = deserialized.info.data.file_tree.as_ref().unwrap();
        let files = collect_v2_files(file_tree);
        assert_eq!(files.len(), 2);

        let empty_file = files.iter().find(|f| f.entry.length == 0).unwrap();
        assert!(
            empty_file.entry.pieces_root.is_none(),
            "empty file should have no pieces_root"
        );

        let normal_file = files.iter().find(|f| f.entry.length > 0).unwrap();
        assert!(
            normal_file.entry.pieces_root.is_some(),
            "normal file should have pieces_root"
        );

        deserialized
            .validate_v2_piece_layers()
            .expect("piece_layers validation should pass");
    }

    #[tokio::test]
    async fn test_create_v2_invalid_piece_length() {
        let dir = tempfile::TempDir::with_prefix("rqbit_test_v2_invalid_pl").unwrap();
        test_util::create_new_file_with_random_content(&dir.path().join("file.bin"), 1000);

        // Non-power-of-two.
        let result = create_torrent(
            dir.path(),
            CreateTorrentOptions {
                version: Some(TorrentVersion::V2Only),
                piece_length: Some(48000),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await;
        assert!(result.is_err(), "non-power-of-two piece_length should fail");

        // Less than 16384.
        let result = create_torrent(
            dir.path(),
            CreateTorrentOptions {
                version: Some(TorrentVersion::V2Only),
                piece_length: Some(8192),
                ..Default::default()
            },
            &BlockingSpawner::new(1),
        )
        .await;
        assert!(result.is_err(), "piece_length < 16384 should fail for v2");
    }
}
