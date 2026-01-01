use std::borrow::Cow;
use std::ffi::OsStr;
use std::io::{BufWriter, Read};
use std::path::{Path, PathBuf};

use anyhow::Context;
use bencode::{WithRawBytes, bencode_serialize_to_writer};
use buffers::ByteBufOwned;
use bytes::Bytes;
use librqbit_core::Id20;
use librqbit_core::magnet::Magnet;
use librqbit_core::torrent_metainfo::{TorrentMetaV1File, TorrentMetaV1Info, TorrentMetaV1Owned};
use sha1w::ISha1;

use crate::spawn_utils::BlockingSpawner;

#[derive(Debug, Clone, Default)]
pub struct CreateTorrentOptions<'a> {
    pub name: Option<&'a str>,
    pub trackers: Vec<String>,
    pub piece_length: Option<u32>,
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

fn compute_info_hash(t: &TorrentMetaV1Info<ByteBufOwned>) -> anyhow::Result<(Id20, Bytes)> {
    let mut writer = BufWriter::new(Vec::new());
    bencode_serialize_to_writer(t, &mut writer)?;
    let bytes: Bytes = writer
        .into_inner()
        .map_err(|_| anyhow::anyhow!("into_inner errored"))?
        .into();
    let hash = Id20::new({
        let mut h = sha1w::Sha1::new();
        h.update(&bytes);
        h.finish()
    });
    Ok((hash, bytes))
}

fn choose_piece_length(_input_files: &[Cow<'_, Path>]) -> u32 {
    // TODO: make this smarter or smth
    2 * 1024 * 1024
}

fn osstr_to_bytes(o: &OsStr) -> Vec<u8> {
    o.to_str().unwrap().to_owned().into_bytes()
}

struct CreateTorrentRawResult {
    info: TorrentMetaV1Info<ByteBufOwned>,
    output_folder: PathBuf,
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
    let single_file_mode = !is_dir;
    let name: ByteBufOwned = match options.name {
        Some(name) => name.as_bytes().into(),
        None => osstr_to_bytes(basename).into(),
    };
    let output_folder: PathBuf;

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

    let piece_length = options
        .piece_length
        .unwrap_or_else(|| choose_piece_length(&input_files));

    // Calculate hashes etc.
    const READ_SIZE: u32 = 8192; // todo: twea
    let mut read_buf = vec![0; READ_SIZE as usize];

    let _permit = spawner.semaphore().acquire_owned().await?;

    let mut length = 0;
    let mut remaining_piece_length = piece_length;
    let mut piece_checksum = sha1w::Sha1::new();
    let mut piece_hashes = Vec::<u8>::new();
    let mut output_files: Vec<TorrentMetaV1File<ByteBufOwned>> = Vec::new();

    'outer: for file in input_files {
        let filename = &*file;
        length = 0;
        let mut fd = std::io::BufReader::new(
            std::fs::File::open(&file).with_context(|| format!("error opening {filename:?}"))?,
        );

        loop {
            let max_bytes_to_read = remaining_piece_length.min(READ_SIZE) as usize;
            // NOTE: we can't use the semaphore as Sha1 isn't Send at least on OSX.
            let size = spawner
                .block_in_place(|| fd.read(&mut read_buf[..max_bytes_to_read]))
                .with_context(|| format!("error reading {filename:?}"))?;

            // EOF: swap file
            if size == 0 {
                let filename = filename
                    .strip_prefix(path)
                    .context("internal error, can't strip prefix")?;
                let path = filename
                    .components()
                    .map(|c| osstr_to_bytes(c.as_os_str()).into())
                    .collect();
                output_files.push(TorrentMetaV1File {
                    length,
                    path,
                    attr: None,
                    sha1: None,
                    symlink_path: None,
                });
                continue 'outer;
            }

            length += size as u64;
            piece_checksum.update(&read_buf[..size]);

            remaining_piece_length -= TryInto::<u32>::try_into(size)?;
            if remaining_piece_length == 0 {
                remaining_piece_length = piece_length;
                piece_hashes.extend_from_slice(&piece_checksum.finish());
                piece_checksum = sha1w::Sha1::new();
            }
        }
    }

    if remaining_piece_length > 0 && length > 0 {
        piece_hashes.extend_from_slice(&piece_checksum.finish());
    }
    Ok(CreateTorrentRawResult {
        info: TorrentMetaV1Info {
            name: Some(name),
            pieces: piece_hashes.into(),
            piece_length,
            length: if single_file_mode { Some(length) } else { None },
            md5sum: None,
            files: if single_file_mode {
                None
            } else {
                Some(output_files)
            },
            attr: None,
            sha1: None,
            symlink_path: None,
            private: false,
        },
        output_folder,
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

    pub fn as_magnet(&self) -> Magnet {
        let trackers = self
            .meta
            .iter_announce()
            .map(|i| std::str::from_utf8(i.as_ref()).unwrap().to_owned())
            .collect();
        Magnet::from_id20(self.info_hash(), trackers, None)
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
    let (info_hash, bytes) = compute_info_hash(&res.info).context("error computing info hash")?;
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
        },
        output_folder: res.output_folder,
    })
}

#[cfg(test)]
mod tests {
    use librqbit_core::torrent_metainfo::torrent_from_bytes;

    use crate::{create_torrent, spawn_utils::BlockingSpawner};

    #[tokio::test]
    async fn test_create_torrent() {
        use crate::tests::test_util;

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
}
