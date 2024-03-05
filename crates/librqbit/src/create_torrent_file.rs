use std::borrow::Cow;
use std::ffi::OsStr;
use std::io::{BufWriter, Read};
use std::path::Path;

use anyhow::Context;
use bencode::bencode_serialize_to_writer;
use buffers::ByteString;
use librqbit_core::torrent_metainfo::{TorrentMetaV1File, TorrentMetaV1Info, TorrentMetaV1Owned};
use librqbit_core::Id20;
use sha1w::{ISha1, Sha1};

use crate::spawn_utils::BlockingSpawner;

#[derive(Debug, Clone, Default)]
pub struct CreateTorrentOptions<'a> {
    pub name: Option<&'a str>,
    pub piece_length: Option<u32>,
}

fn walk_dir_find_paths(dir: &Path, out: &mut Vec<Cow<'_, Path>>) -> anyhow::Result<()> {
    let mut stack = vec![Cow::Borrowed(dir)];
    while let Some(dir) = stack.pop() {
        let rd = std::fs::read_dir(&dir).with_context(|| format!("error reading {:?}", dir))?;
        for element in rd {
            let element =
                element.with_context(|| format!("error reading DirEntry from {:?}", dir))?;
            let ft = element.file_type().with_context(|| {
                format!(
                    "error determining filetype of DirEntry {:?} while reading {:?}",
                    element.file_name(),
                    dir
                )
            })?;

            let full_path = Cow::Owned(dir.join(element.file_name()));
            if ft.is_dir() {
                stack.push(full_path);
            } else {
                out.push(full_path);
            }
        }
    }
    Ok(())
}

fn compute_info_hash(t: &TorrentMetaV1Info<ByteString>) -> anyhow::Result<Id20> {
    struct W {
        hash: sha1w::Sha1,
    }
    impl std::io::Write for W {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.hash.update(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = BufWriter::new(W { hash: Sha1::new() });
    bencode_serialize_to_writer(t, &mut writer)?;
    let hash = writer
        .into_inner()
        .map_err(|_| anyhow::anyhow!("into_inner errored"))?
        .hash;
    Ok(Id20::new(hash.finish()))
}

fn choose_piece_length(_input_files: &[Cow<'_, Path>]) -> u32 {
    // TODO: make this smarter or smth
    2 * 1024 * 1024
}

fn osstr_to_bytes(o: &OsStr) -> Vec<u8> {
    o.to_str().unwrap().to_owned().into_bytes()
}

async fn create_torrent_raw<'a>(
    path: &'a Path,
    options: CreateTorrentOptions<'a>,
) -> anyhow::Result<TorrentMetaV1Info<ByteString>> {
    path.try_exists()
        .with_context(|| format!("path {:?} doesn't exist", path))?;
    let basename = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("cannot determine basename of {:?}", path))?;
    let is_dir = path.is_dir();
    let single_file_mode = !is_dir;
    let name: ByteString = match options.name {
        Some(name) => name.as_bytes().into(),
        None => osstr_to_bytes(basename).into(),
    };

    let mut input_files: Vec<Cow<'a, Path>> = Default::default();
    if is_dir {
        walk_dir_find_paths(path, &mut input_files)
            .with_context(|| format!("error walking {:?}", path))?;
    } else {
        input_files.push(Cow::Borrowed(path));
    }

    let piece_length = options
        .piece_length
        .unwrap_or_else(|| choose_piece_length(&input_files));

    // Calculate hashes etc.
    const READ_SIZE: u32 = 8192; // todo: twea
    let mut read_buf = vec![0; READ_SIZE as usize];

    let mut length = 0;
    let mut remaining_piece_length = piece_length;
    let mut piece_checksum = sha1w::Sha1::new();
    let mut piece_hashes = Vec::<u8>::new();
    let mut output_files: Vec<TorrentMetaV1File<ByteString>> = Vec::new();

    let spawner = BlockingSpawner::default();

    'outer: for file in input_files {
        let filename = &*file;
        length = 0;
        let mut fd = std::io::BufReader::new(
            std::fs::File::open(&file).with_context(|| format!("error opening {:?}", filename))?,
        );

        loop {
            let max_bytes_to_read = remaining_piece_length.min(READ_SIZE) as usize;
            let size = spawner
                .spawn_block_in_place(|| fd.read(&mut read_buf[..max_bytes_to_read]))
                .with_context(|| format!("error reading {:?}", filename))?;

            // EOF: swap file
            if size == 0 {
                let filename = filename
                    .strip_prefix(path)
                    .context("internal error, can't strip prefix")?;
                let path = filename
                    .components()
                    .map(|c| osstr_to_bytes(c.as_os_str()).into())
                    .collect();
                output_files.push(TorrentMetaV1File { length, path });
                continue 'outer;
            }

            length += size as u64;
            piece_checksum.update(&read_buf[..size]);

            remaining_piece_length -= size as u32;
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
    Ok(TorrentMetaV1Info {
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
    })
}

#[derive(Debug)]
pub struct CreateTorrentResult {
    meta: TorrentMetaV1Owned,
}

impl CreateTorrentResult {
    pub fn as_info(&self) -> &TorrentMetaV1Owned {
        &self.meta
    }

    pub fn info_hash(&self) -> Id20 {
        self.meta.info_hash
    }

    pub fn as_bytes(&self) -> anyhow::Result<Vec<u8>> {
        let mut b = Vec::new();
        bencode_serialize_to_writer(&self.meta, &mut b).context("error serializing torrent")?;
        Ok(b)
    }
}

pub async fn create_torrent<'a>(
    path: &'a Path,
    options: CreateTorrentOptions<'a>,
) -> anyhow::Result<CreateTorrentResult> {
    let info = create_torrent_raw(path, options).await?;
    let info_hash = compute_info_hash(&info).context("error computing info hash")?;
    Ok(CreateTorrentResult {
        meta: TorrentMetaV1Owned {
            announce: b""[..].into(),
            announce_list: Vec::new(),
            info,
            comment: None,
            created_by: None,
            encoding: Some(b"utf-8"[..].into()),
            publisher: None,
            publisher_url: None,
            creation_date: None,
            info_hash,
        },
    })
}

#[cfg(test)]
mod tests {
    use buffers::ByteBuf;
    use librqbit_core::torrent_metainfo::torrent_from_bytes;

    use crate::create_torrent;

    #[tokio::test]
    async fn test_create_torrent() {
        use crate::tests::test_util;

        let dir = test_util::create_default_random_dir_with_torrents(
            3,
            1000 * 1000,
            Some("rqbit_test_create_torrent"),
        );
        let torrent = create_torrent(dir.path(), Default::default())
            .await
            .unwrap();

        let bytes = torrent.as_bytes().unwrap();

        let deserialized = torrent_from_bytes::<ByteBuf>(&bytes).unwrap();
        assert_eq!(torrent.info_hash(), deserialized.info_hash);
    }
}
