use std::{iter::once, path::PathBuf};

use anyhow::Context;
use bencode::BencodeDeserializer;
use buffers::{ByteBuf, ByteString};
use clone_to_owned::CloneToOwned;
use itertools::Either;
use serde::{Deserialize, Serialize};

use crate::hash_id::Id20;

pub type TorrentMetaV1Borrowed<'a> = TorrentMetaV1<ByteBuf<'a>>;
pub type TorrentMetaV1Owned = TorrentMetaV1<ByteString>;

/// Parse torrent metainfo from bytes.
pub fn torrent_from_bytes<'de, ByteBuf: Deserialize<'de>>(
    buf: &'de [u8],
) -> anyhow::Result<TorrentMetaV1<ByteBuf>> {
    let mut de = BencodeDeserializer::new_from_buf(buf);
    de.is_torrent_info = true;
    let mut t = TorrentMetaV1::deserialize(&mut de)?;
    t.info_hash = Id20::new(
        de.torrent_info_digest
            .ok_or_else(|| anyhow::anyhow!("programming error"))?,
    );
    Ok(t)
}

/// A parsed .torrent file.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TorrentMetaV1<BufType> {
    pub announce: BufType,
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
        itertools::Either::Right(once(&self.announce))
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
pub enum FileIteratorName<'a, ByteBuf> {
    Single(Option<&'a ByteBuf>),
    Tree(&'a [ByteBuf]),
}

impl<'a, ByteBuf> std::fmt::Debug for FileIteratorName<'a, ByteBuf>
where
    ByteBuf: AsRef<[u8]>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.to_string() {
            Ok(s) => write!(f, "{s:?}"),
            Err(e) => write!(f, "<{e:?}>"),
        }
    }
}

impl<'a, ByteBuf> FileIteratorName<'a, ByteBuf>
where
    ByteBuf: AsRef<[u8]>,
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
            if bit.contains(std::path::MAIN_SEPARATOR) {
                anyhow::bail!(
                    "suspicios separator {:?} in filename bit {:?}",
                    std::path::MAIN_SEPARATOR,
                    bit
                );
            }
            Ok(bit)
        }))
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

impl<ByteBuf> CloneToOwned for TorrentMetaV1File<ByteBuf>
where
    ByteBuf: CloneToOwned,
{
    type Target = TorrentMetaV1File<<ByteBuf as CloneToOwned>::Target>;

    fn clone_to_owned(&self) -> Self::Target {
        TorrentMetaV1File {
            length: self.length,
            path: self.path.clone_to_owned(),
        }
    }
}

impl<ByteBuf> CloneToOwned for TorrentMetaV1Info<ByteBuf>
where
    ByteBuf: CloneToOwned,
{
    type Target = TorrentMetaV1Info<<ByteBuf as CloneToOwned>::Target>;

    fn clone_to_owned(&self) -> Self::Target {
        TorrentMetaV1Info {
            name: self.name.clone_to_owned(),
            pieces: self.pieces.clone_to_owned(),
            piece_length: self.piece_length,
            length: self.length,
            md5sum: self.md5sum.clone_to_owned(),
            files: self.files.clone_to_owned(),
        }
    }
}

impl<ByteBuf> CloneToOwned for TorrentMetaV1<ByteBuf>
where
    ByteBuf: CloneToOwned,
{
    type Target = TorrentMetaV1<<ByteBuf as CloneToOwned>::Target>;

    fn clone_to_owned(&self) -> Self::Target {
        TorrentMetaV1 {
            announce: self.announce.clone_to_owned(),
            announce_list: self.announce_list.clone_to_owned(),
            info: self.info.clone_to_owned(),
            comment: self.comment.clone_to_owned(),
            created_by: self.created_by.clone_to_owned(),
            encoding: self.encoding.clone_to_owned(),
            publisher: self.publisher.clone_to_owned(),
            publisher_url: self.publisher_url.clone_to_owned(),
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
