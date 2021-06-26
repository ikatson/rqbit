use std::{fmt::Write, fs::File, ops::Deref, path::PathBuf};

use serde::Deserialize;

use crate::{
    buffers::{ByteBuf, ByteString},
    clone_to_owned::CloneToOwned,
    serde_bencode::BencodeDeserializer,
};

pub type TorrentMetaV1Borrowed<'a> = TorrentMetaV1<ByteBuf<'a>>;
pub type TorrentMetaV1Owned = TorrentMetaV1<ByteString>;

pub fn torrent_from_bytes(buf: &[u8]) -> anyhow::Result<TorrentMetaV1Borrowed<'_>> {
    let mut de = BencodeDeserializer::new_from_buf(buf);
    de.is_torrent_info = true;
    let mut t = TorrentMetaV1::deserialize(&mut de)?;
    t.info_hash = de.torrent_info_digest.unwrap();
    Ok(t)
}

pub fn torrent_from_bytes_owned(buf: &[u8]) -> anyhow::Result<TorrentMetaV1Owned> {
    let mut de = BencodeDeserializer::new_from_buf(buf);
    de.is_torrent_info = true;
    let mut t = TorrentMetaV1Owned::deserialize(&mut de)?;
    t.info_hash = de.torrent_info_digest.unwrap();
    Ok(t)
}

#[derive(Deserialize, Debug, Clone)]
pub struct TorrentMetaV1<BufType: Clone> {
    pub announce: BufType,
    #[serde(rename = "announce-list")]
    pub announce_list: Vec<Vec<BufType>>,
    pub info: TorrentMetaV1Info<BufType>,
    pub comment: Option<BufType>,
    #[serde(rename = "created by")]
    pub created_by: Option<BufType>,
    pub encoding: Option<BufType>,
    pub publisher: Option<BufType>,
    #[serde(rename = "publisher-url")]
    pub publisher_url: Option<BufType>,
    #[serde(rename = "creation date")]
    pub creation_date: Option<usize>,

    #[serde(skip)]
    pub info_hash: [u8; 20],
}

impl<BufType: Clone> TorrentMetaV1<BufType> {
    pub fn iter_announce(&self) -> impl Iterator<Item = &BufType> {
        std::iter::once(&self.announce).chain(self.announce_list.iter().flatten())
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct TorrentMetaV1Info<BufType: Clone> {
    pub name: Option<BufType>,
    pub pieces: BufType,
    #[serde(rename = "piece length")]
    pub piece_length: u32,

    // Single-file mode
    pub length: Option<u64>,
    pub md5sum: Option<BufType>,

    // Multi-file mode
    pub files: Option<Vec<TorrentMetaV1File<BufType>>>,
}

pub enum FileIteratorName<'a, ByteBuf> {
    Single(Option<&'a ByteBuf>),
    Tree(&'a [ByteBuf]),
}

impl<'a, ByteBuf> std::fmt::Debug for FileIteratorName<'a, ByteBuf>
where
    ByteBuf: AsRef<[u8]>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (idx, item) in self.iter_components().enumerate() {
            if idx > 0 {
                f.write_char(std::path::MAIN_SEPARATOR)?;
            }
            match item {
                Some(bit) => {
                    f.write_str(std::str::from_utf8(bit.as_ref()).unwrap_or("<INVALID UTF-8>"))?;
                }
                None => f.write_str("output")?,
            }
        }
        Ok(())
    }
}

impl<'a, ByteBuf> FileIteratorName<'a, ByteBuf> {
    pub fn to_pathbuf(&self) -> anyhow::Result<PathBuf>
    where
        ByteBuf: AsRef<[u8]>,
    {
        let mut buf = PathBuf::new();
        for part in self.iter_components() {
            if let Some(part) = part {
                buf.push(std::str::from_utf8(part.as_ref())?)
            } else {
                buf.push("output");
                break;
            }
        }
        Ok(buf)
    }
    pub fn iter_components(&self) -> impl Iterator<Item = Option<&'a ByteBuf>> {
        let single_it = std::iter::once(match self {
            FileIteratorName::Single(n) => Some(*n),
            FileIteratorName::Tree(_) => None,
        });
        let multi_it = match self {
            FileIteratorName::Single(_) => &[],
            FileIteratorName::Tree(t) => *t,
        }
        .iter()
        .map(|p| Some(Some(p)));

        single_it.chain(multi_it).flatten()
    }
}

impl<BufType: Clone + Deref<Target = [u8]>> TorrentMetaV1Info<BufType> {
    pub fn get_hash(&self, piece: u32, hash: &sha1::Sha1) -> Option<&[u8]> {
        let start = piece as usize * 20;
        let end = start + 20;
        let expected_hash = self.pieces.deref().get(start..end)?;
        Some(expected_hash)
    }
    pub fn compare_hash(&self, piece: u32, hash: &sha1::Sha1) -> Option<bool> {
        let start = piece as usize * 20;
        let end = start + 20;
        let expected_hash = self.pieces.deref().get(start..end)?;
        Some(expected_hash == hash.digest().bytes())
    }
    pub fn iter_filenames_and_lengths(
        &self,
    ) -> impl Iterator<Item = (FileIteratorName<'_, BufType>, u64)> {
        let single_it = std::iter::once(match (self.name.as_ref(), self.length) {
            (Some(n), Some(l)) => Some((FileIteratorName::Single(Some(n)), l)),
            _ => None,
        });
        let multi_it = self
            .files
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|f| Some((FileIteratorName::Tree(&f.path), f.length)));
        single_it.chain(multi_it).flatten()
    }
    pub fn iter_file_lengths(&self) -> impl Iterator<Item = u64> + '_ {
        std::iter::once(self.length)
            .chain(
                self.files
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(|f| Some(f.length)),
            )
            .flatten()
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct TorrentMetaV1File<BufType: Clone> {
    pub length: u64,
    pub path: Vec<BufType>,
}

impl<BufType> TorrentMetaV1File<BufType>
where
    BufType: Clone + AsRef<[u8]>,
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
    ByteBuf: CloneToOwned + Clone,
    <ByteBuf as CloneToOwned>::Target: Clone,
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
    ByteBuf: CloneToOwned + Clone,
    <ByteBuf as CloneToOwned>::Target: Clone,
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
    ByteBuf: CloneToOwned + Clone,
    <ByteBuf as CloneToOwned>::Target: Clone,
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

    use crate::serde_bencode::from_bytes;

    use super::*;

    #[test]
    fn test_deserialize_torrent_owned() {
        let mut buf = Vec::new();
        let filename = "resources/ubuntu-21.04-desktop-amd64.iso.torrent";
        std::fs::File::open(filename)
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();

        let torrent: TorrentMetaV1Owned = from_bytes(&buf).unwrap();
        dbg!(torrent);
    }

    #[test]
    fn test_deserialize_torrent_borrowed() {
        let mut buf = Vec::new();
        let filename = "resources/ubuntu-21.04-desktop-amd64.iso.torrent";
        std::fs::File::open(filename)
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();

        let torrent: TorrentMetaV1Borrowed = from_bytes(&buf).unwrap();
        dbg!(torrent);
    }

    #[test]
    fn test_deserialize_torrent_with_info_hash() {
        let mut buf = Vec::new();
        let filename = "resources/ubuntu-21.04-desktop-amd64.iso.torrent";
        std::fs::File::open(filename)
            .unwrap()
            .read_to_end(&mut buf)
            .unwrap();

        let torrent = torrent_from_bytes(&buf).unwrap();
        assert_eq!(
            torrent.info_hash,
            *b"\x64\xa9\x80\xab\xe6\xe4\x48\x22\x6b\xb9\x30\xba\x06\x15\x92\xe4\x4c\x37\x81\xa1"
        );
    }
}
