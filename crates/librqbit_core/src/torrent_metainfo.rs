use anyhow::Context;
use bencode::BencodeDeserializer;
use buffers::{ByteBuf, ByteBufOwned};
use bytes::Bytes;
use clone_to_owned::CloneToOwned;
use itertools::Either;
use serde::{Deserialize, Serialize};
use std::{iter::once, path::PathBuf};

use crate::{hash_id::Id20, lengths::Lengths};

pub type TorrentMetaV1Borrowed<'a> = TorrentMetaV1<ByteBuf<'a>>;
pub type TorrentMetaV1Owned = TorrentMetaV1<ByteBufOwned>;

pub struct ParsedTorrent<BufType> {
    /// The parsed torrent.
    pub meta: TorrentMetaV1<BufType>,

    /// The raw bytes of the torrent's "info" dict.
    pub info_bytes: BufType,
}

/// Parse torrent metainfo from bytes (includes additional fields).
pub fn torrent_from_bytes_ext<'de, BufType: Deserialize<'de> + From<&'de [u8]>>(
    buf: &'de [u8],
) -> anyhow::Result<ParsedTorrent<BufType>> {
    let mut de = BencodeDeserializer::new_from_buf(buf);
    de.is_torrent_info = true;
    let mut t = TorrentMetaV1::deserialize(&mut de)?;
    let (digest, info_bytes) = match (de.torrent_info_digest, de.torrent_info_bytes) {
        (Some(digest), Some(info_bytes)) => (digest, info_bytes),
        _ => anyhow::bail!("programming error"),
    };
    t.info_hash = Id20::new(digest);
    Ok(ParsedTorrent {
        meta: t,
        info_bytes: BufType::from(info_bytes),
    })
}

/// Parse torrent metainfo from bytes.
pub fn torrent_from_bytes<'de, BufType: Deserialize<'de> + From<&'de [u8]>>(
    buf: &'de [u8],
) -> anyhow::Result<TorrentMetaV1<BufType>> {
    torrent_from_bytes_ext(buf).map(|r| r.meta)
}

/// A parsed .torrent file.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TorrentMetaV1<BufType> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announce: Option<BufType>,
    #[serde(
        rename = "announce-list",
        default = "Vec::new",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub announce_list: Vec<Vec<BufType>>,
    pub info: TorrentMetaV1Info<BufType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<BufType>,
    #[serde(rename = "created by", skip_serializing_if = "Option::is_none")]
    pub created_by: Option<BufType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<BufType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<BufType>,
    #[serde(rename = "publisher-url", skip_serializing_if = "Option::is_none")]
    pub publisher_url: Option<BufType>,
    #[serde(rename = "creation date", skip_serializing_if = "Option::is_none")]
    pub creation_date: Option<usize>,

    #[serde(skip)]
    pub info_hash: Id20,
}

impl<BufType> TorrentMetaV1<BufType> {
    pub fn iter_announce(&self) -> impl Iterator<Item = &BufType> {
        if self.announce_list.iter().flatten().next().is_some() {
            return itertools::Either::Left(self.announce_list.iter().flatten());
        }
        itertools::Either::Right(self.announce.iter())
    }
}

/// Main torrent information, shared by .torrent files and magnet link contents.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TorrentMetaV1Info<BufType> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<BufType>,
    pub pieces: BufType,
    #[serde(rename = "piece length")]
    pub piece_length: u32,

    // Single-file mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub md5sum: Option<BufType>,

    // Multi-file mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<TorrentMetaV1File<BufType>>>,
}

#[derive(Clone, Copy)]
pub enum FileIteratorName<'a, BufType> {
    Single(Option<&'a BufType>),
    Tree(&'a [BufType]),
}

impl<'a, BufType> std::fmt::Debug for FileIteratorName<'a, BufType>
where
    BufType: AsRef<[u8]>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.to_string() {
            Ok(s) => write!(f, "{s:?}"),
            Err(e) => write!(f, "<{e:?}>"),
        }
    }
}

impl<'a, BufType> FileIteratorName<'a, BufType>
where
    BufType: AsRef<[u8]>,
{
    pub fn to_vec(&self) -> anyhow::Result<Vec<String>> {
        self.iter_components()
            .map(|c| c.map(|s| s.to_owned()))
            .collect()
    }

    pub fn to_string(&self) -> anyhow::Result<String> {
        let mut buf = String::new();
        for (idx, bit) in self.iter_components().enumerate() {
            let bit = bit?;
            if idx > 0 {
                buf.push(std::path::MAIN_SEPARATOR);
            }
            buf.push_str(bit)
        }
        Ok(buf)
    }
    pub fn to_pathbuf(&self) -> anyhow::Result<PathBuf> {
        let mut buf = PathBuf::new();
        for bit in self.iter_components() {
            let bit = bit?;
            buf.push(bit)
        }
        Ok(buf)
    }
    pub fn iter_components(&self) -> impl Iterator<Item = anyhow::Result<&'a str>> {
        let it = match self {
            FileIteratorName::Single(None) => return Either::Left(once(Ok("torrent-content"))),
            FileIteratorName::Single(Some(name)) => Either::Left(once((*name).as_ref())),
            FileIteratorName::Tree(t) => Either::Right(t.iter().map(|bb| bb.as_ref())),
        };
        Either::Right(it.map(|part: &'a [u8]| {
            let bit = std::str::from_utf8(part).context("cannot decode filename bit as UTF-8")?;
            if bit == ".." {
                anyhow::bail!("path traversal detected, \"..\" in filename bit {:?}", bit);
            }
            if bit.contains('/') || bit.contains('\\') {
                anyhow::bail!("suspicios separator in filename bit {:?}", bit);
            }
            Ok(bit)
        }))
    }
}

pub struct FileDetails<'a, BufType> {
    pub filename: FileIteratorName<'a, BufType>,
    pub offset: u64,
    pub len: u64,
    pub pieces: std::ops::Range<u32>,
}

impl<'a, BufType> FileDetails<'a, BufType> {
    pub fn pieces_usize(&self) -> std::ops::Range<usize> {
        self.pieces.start as usize..self.pieces.end as usize
    }
}

impl<BufType: AsRef<[u8]>> TorrentMetaV1Info<BufType> {
    pub fn get_hash(&self, piece: u32) -> Option<&[u8]> {
        let start = piece as usize * 20;
        let end = start + 20;
        let expected_hash = self.pieces.as_ref().get(start..end)?;
        Some(expected_hash)
    }

    pub fn compare_hash(&self, piece: u32, hash: [u8; 20]) -> Option<bool> {
        let start = piece as usize * 20;
        let end = start + 20;
        let expected_hash = self.pieces.as_ref().get(start..end)?;
        Some(expected_hash == hash)
    }

    #[inline(never)]
    pub fn iter_filenames_and_lengths(
        &self,
    ) -> anyhow::Result<impl Iterator<Item = (FileIteratorName<'_, BufType>, u64)>> {
        match (self.length, self.files.as_ref()) {
            // Single-file
            (Some(length), None) => Ok(Either::Left(once((
                FileIteratorName::Single(self.name.as_ref()),
                length,
            )))),

            // Multi-file
            (None, Some(files)) => {
                if files.is_empty() {
                    anyhow::bail!("expected multi-file torrent to have at least one file")
                }
                Ok(Either::Right(
                    files
                        .iter()
                        .map(|f| (FileIteratorName::Tree(&f.path), f.length)),
                ))
            }
            _ => anyhow::bail!("torrent can't be both in single and multi-file mode"),
        }
    }

    pub fn iter_file_lengths(&self) -> anyhow::Result<impl Iterator<Item = u64> + '_> {
        Ok(self.iter_filenames_and_lengths()?.map(|(_, l)| l))
    }

    // NOTE: lenghts MUST be construced with Lenghts::from_torrent, otherwise
    // the yielded results will be garbage.
    pub fn iter_file_details<'a>(
        &'a self,
        lengths: &'a Lengths,
    ) -> anyhow::Result<impl Iterator<Item = FileDetails<'a, BufType>> + 'a> {
        Ok(self
            .iter_filenames_and_lengths()?
            .scan(0u64, |acc_offset, (filename, len)| {
                let offset = *acc_offset;
                *acc_offset += len;
                Some(FileDetails {
                    filename,
                    pieces: lengths.iter_pieces_within_offset(offset, len),
                    offset,
                    len,
                })
            }))
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct TorrentMetaV1File<BufType> {
    pub length: u64,
    pub path: Vec<BufType>,
}

impl<BufType> TorrentMetaV1File<BufType>
where
    BufType: AsRef<[u8]>,
{
    pub fn full_path(&self, parent: &mut PathBuf) -> anyhow::Result<()> {
        for p in self.path.iter() {
            let bit = std::str::from_utf8(p.as_ref())?;
            parent.push(bit);
        }
        Ok(())
    }
}

impl<BufType> CloneToOwned for TorrentMetaV1File<BufType>
where
    BufType: CloneToOwned,
{
    type Target = TorrentMetaV1File<<BufType as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        TorrentMetaV1File {
            length: self.length,
            path: self.path.clone_to_owned(within_buffer),
        }
    }
}

impl<BufType> CloneToOwned for TorrentMetaV1Info<BufType>
where
    BufType: CloneToOwned,
{
    type Target = TorrentMetaV1Info<<BufType as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        TorrentMetaV1Info {
            name: self.name.clone_to_owned(within_buffer),
            pieces: self.pieces.clone_to_owned(within_buffer),
            piece_length: self.piece_length,
            length: self.length,
            md5sum: self.md5sum.clone_to_owned(within_buffer),
            files: self.files.clone_to_owned(within_buffer),
        }
    }
}

impl<BufType> CloneToOwned for TorrentMetaV1<BufType>
where
    BufType: CloneToOwned,
{
    type Target = TorrentMetaV1<<BufType as CloneToOwned>::Target>;

    fn clone_to_owned(&self, within_buffer: Option<&Bytes>) -> Self::Target {
        TorrentMetaV1 {
            announce: self.announce.clone_to_owned(within_buffer),
            announce_list: self.announce_list.clone_to_owned(within_buffer),
            info: self.info.clone_to_owned(within_buffer),
            comment: self.comment.clone_to_owned(within_buffer),
            created_by: self.created_by.clone_to_owned(within_buffer),
            encoding: self.encoding.clone_to_owned(within_buffer),
            publisher: self.publisher.clone_to_owned(within_buffer),
            publisher_url: self.publisher_url.clone_to_owned(within_buffer),
            creation_date: self.creation_date,
            info_hash: self.info_hash,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    const TORRENT_FILENAME: &str = "../librqbit/resources/ubuntu-21.04-desktop-amd64.iso.torrent";

    #[test]
    fn test_deserialize_torrent_owned() {
        let mut buf = Vec::new();
        std::fs::File::open(TORRENT_FILENAME)
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();

        let torrent: TorrentMetaV1Owned = torrent_from_bytes(&buf).unwrap();
        dbg!(torrent);
    }

    #[test]
    fn test_deserialize_torrent_borrowed() {
        let mut buf = Vec::new();
        std::fs::File::open(TORRENT_FILENAME)
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();

        let torrent: TorrentMetaV1Borrowed = torrent_from_bytes(&buf).unwrap();
        dbg!(torrent);
    }

    #[test]
    fn test_deserialize_torrent_with_info_hash() {
        let mut buf = Vec::new();
        std::fs::File::open(TORRENT_FILENAME)
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();

        let torrent: TorrentMetaV1Borrowed = torrent_from_bytes(&buf).unwrap();
        assert_eq!(
            torrent.info_hash.as_string(),
            "64a980abe6e448226bb930ba061592e44c3781a1"
        );
    }

    #[test]
    fn test_serialize_then_deserialize_bencode() {
        let mut buf = Vec::new();
        std::fs::File::open(TORRENT_FILENAME)
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();

        let torrent: TorrentMetaV1Info<ByteBuf> = torrent_from_bytes(&buf).unwrap().info;
        let mut writer = Vec::new();
        bencode::bencode_serialize_to_writer(&torrent, &mut writer).unwrap();
        let deserialized = TorrentMetaV1Info::<ByteBuf>::deserialize(
            &mut BencodeDeserializer::new_from_buf(&writer),
        )
        .unwrap();

        assert_eq!(torrent, deserialized);
    }
}
