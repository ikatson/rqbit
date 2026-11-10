use bencode::WithRawBytes;
use buffers::{ByteBuf, ByteBufOwned};
use bytes::Bytes;
use clone_to_owned::CloneToOwned;
use encoding_rs::Encoding;
use itertools::Either;
use serde_derive::{Deserialize, Serialize};
use std::{borrow::Cow, collections::HashSet, iter::once, path::PathBuf};
use tracing::debug;

use crate::{Error, hash_id::Id20, lengths::Lengths};

pub type TorrentMetaV1Borrowed<'a> = TorrentMetaV1<ByteBuf<'a>>;
pub type TorrentMetaV1Owned = TorrentMetaV1<ByteBufOwned>;

pub struct ParsedTorrent<BufType> {
    /// The parsed torrent.
    pub meta: TorrentMetaV1<BufType>,

    /// The raw bytes of the torrent's "info" dict.
    pub info_bytes: BufType,
}

/// Parse torrent metainfo from bytes (includes info_hash).
#[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
pub fn torrent_from_bytes<'de>(
    buf: &'de [u8],
) -> Result<TorrentMetaV1<ByteBuf<'de>>, bencode::DeserializeError> {
    let mut t: TorrentMetaV1<ByteBuf<'_>> = bencode::from_bytes(buf)
        .inspect_err(|e| tracing::trace!("error deserializing torrent: {e:#}"))
        .map_err(|e| e.into_kind())?;

    use sha1w::ISha1;

    let mut digest = sha1w::Sha1::new();
    digest.update(t.info.raw_bytes.as_ref());
    t.info_hash = Id20::new(digest.finish());
    Ok(t)
}

fn is_false(b: &bool) -> bool {
    !*b
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
    pub info: WithRawBytes<TorrentMetaV1Info<BufType>, BufType>,
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
#[derive(Default, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TorrentMetaV1Info<BufType> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<BufType>,
    pub pieces: BufType,
    #[serde(rename = "piece length")]
    pub piece_length: u32,

    // Single-file mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub attr: Option<BufType>,
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub sha1: Option<BufType>,
    #[serde(
        default = "none",
        rename = "symlink path",
        skip_serializing_if = "Option::is_none"
    )]
    pub symlink_path: Option<Vec<BufType>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub md5sum: Option<BufType>,

    // Multi-file mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<TorrentMetaV1File<BufType>>>,

    #[serde(skip_serializing_if = "is_false", default)]
    pub private: bool,
}

#[derive(Clone, Copy)]
pub struct FileIteratorName<'a, BufType> {
    encoding: &'static Encoding,
    data: FileIteratorNameData<'a, BufType>,
}

#[derive(Clone, Copy)]
pub enum FileIteratorNameData<'a, BufType> {
    Single(Option<&'a BufType>),
    Tree(&'a [BufType]),
}

impl<BufType> std::fmt::Debug for FileIteratorName<'_, BufType>
where
    BufType: AsRef<[u8]>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        <Self as std::fmt::Display>::fmt(self, f)
    }
}

impl<BufType> std::fmt::Display for FileIteratorName<'_, BufType>
where
    BufType: AsRef<[u8]>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (idx, bit) in self.iter_components().enumerate() {
            if idx > 0 {
                write!(f, "{}", std::path::MAIN_SEPARATOR)?;
            }
            write!(f, "{bit}")?;
        }
        Ok(())
    }
}

impl<'a, BufType> FileIteratorName<'a, BufType>
where
    BufType: AsRef<[u8]>,
{
    /// Convert path components into a vector.
    pub fn to_vec(&self) -> Vec<String> {
        self.iter_components().map(|c| c.into_owned()).collect()
    }

    /// Convert path components into a PathBuf, for use with local FS.
    pub fn to_pathbuf(&self) -> PathBuf {
        let mut buf = PathBuf::new();
        for bit in self.iter_components() {
            buf.push(&*bit)
        }
        buf
    }

    /// Iterate path components as strings. Will decode using the detected encoding, replacing
    /// unknown characters with a placeholder.
    pub fn iter_components(&self) -> impl Iterator<Item = Cow<'a, str>> + use<'a, BufType> {
        let encoding = self.encoding;
        self.iter_components_bytes()
            .map(move |part| encoding.decode(part).0)
    }

    /// Iterate path components as bytes.
    pub fn iter_components_bytes(&self) -> impl Iterator<Item = &'a [u8]> + use<'a, BufType> {
        let it = match self.data {
            FileIteratorNameData::Single(None) => {
                return Either::Left(once(&b"torrent-content"[..]));
            }
            FileIteratorNameData::Single(Some(name)) => Either::Left(once((*name).as_ref())),
            FileIteratorNameData::Tree(t) => Either::Right(t.iter().map(|bb| bb.as_ref())),
        };
        Either::Right(it)
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, Copy)]
pub struct FileDetailsAttrs {
    pub symlink: bool,
    pub hidden: bool,
    pub padding: bool,
    pub executable: bool,
}

pub struct FileDetails<'a, BufType> {
    pub filename: FileIteratorName<'a, BufType>,
    pub len: u64,

    // bep-47
    attr: Option<&'a BufType>,
    pub sha1: Option<&'a BufType>,
    pub symlink_path: Option<&'a [BufType]>,
}

impl<BufType> FileDetails<'_, BufType>
where
    BufType: AsRef<[u8]>,
{
    pub fn attrs(&self) -> FileDetailsAttrs {
        let attrs = match self.attr {
            Some(attrs) => attrs,
            None => return FileDetailsAttrs::default(),
        };
        let mut result = FileDetailsAttrs::default();
        for byte in attrs.as_ref().iter().copied() {
            match byte {
                b'l' => result.symlink = true,
                b'h' => result.hidden = true,
                b'p' => result.padding = true,
                b'x' => result.executable = true,
                other => debug!(attr = other, "unknown file attribute"),
            }
        }
        result
    }
}

pub struct FileDetailsExt<'a, BufType> {
    pub details: FileDetails<'a, BufType>,
    // absolute offset in torrent if it was a flat blob of bytes
    pub offset: u64,

    // the pieces that contain this file
    pub pieces: std::ops::Range<u32>,
}

impl<BufType> FileDetailsExt<'_, BufType> {
    pub fn pieces_usize(&self) -> std::ops::Range<usize> {
        self.pieces.start as usize..self.pieces.end as usize
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedTorrentMetaV1Info<BufType> {
    encoding: &'static Encoding,
    lengths: Lengths,
    info: TorrentMetaV1Info<BufType>,
}

impl<BufType: AsRef<[u8]>> ValidatedTorrentMetaV1Info<BufType> {
    pub fn name(&self) -> Option<Cow<'_, str>> {
        self.info
            .name
            .as_ref()
            .map(|n| self.encoding.decode(n.as_ref()).0)
            .filter(|n| !n.is_empty())
    }

    pub fn info(&self) -> &TorrentMetaV1Info<BufType> {
        &self.info
    }

    pub fn lengths(&self) -> &Lengths {
        &self.lengths
    }

    pub fn name_or_else<'a, DefaultT: Into<Cow<'a, str>>>(
        &'a self,
        default: impl Fn() -> DefaultT,
    ) -> Cow<'a, str> {
        self.name().unwrap_or_else(|| default().into())
    }

    /// Guaranteed to produce at least one file.
    pub fn iter_file_details(&self) -> impl Iterator<Item = FileDetails<'_, BufType>> {
        // .unwrap is ok here() as we checked errors at creation time.
        self.info.iter_file_details_raw(self.encoding).unwrap()
    }

    pub fn iter_file_lengths(&self) -> impl Iterator<Item = u64> + '_ {
        self.iter_file_details().map(|d| d.len)
    }

    // Iterate file details with additional computations for offsets.
    pub fn iter_file_details_ext<'a>(
        &'a self,
    ) -> impl Iterator<Item = FileDetailsExt<'a, BufType>> + 'a {
        self.iter_file_details()
            .scan(0u64, move |acc_offset, details| {
                let offset = *acc_offset;
                *acc_offset += details.len;
                Some(FileDetailsExt {
                    pieces: self.lengths.iter_pieces_within_offset(offset, details.len),
                    details,
                    offset,
                })
            })
    }
}

impl<BufType: AsRef<[u8]>> TorrentMetaV1Info<BufType> {
    pub fn validate(self) -> crate::Result<ValidatedTorrentMetaV1Info<BufType>> {
        let lengths = Lengths::from_torrent(&self)?;
        let encoding = self.detect_encoding();
        let validated = ValidatedTorrentMetaV1Info {
            encoding,
            lengths,
            info: self,
        };

        // Ensure:
        // - there's at least one file
        // - each filename has at least one path component
        // - there's no path traversal
        // - all non-padding filenames are unique
        let mut seen_files = 0;
        for file in validated.info.iter_file_details_raw(encoding)? {
            seen_files += 1;
            let mut seen_a_bit = false;
            for bit in file.filename.iter_components_bytes() {
                seen_a_bit = true;
                if bit == b".." {
                    return Err(Error::BadTorrentPathTraversal);
                }
                use memchr::memchr;
                if memchr(b'/', bit).is_some() || memchr(b'\\', bit).is_some() {
                    return Err(Error::BadTorrentSeparatorInName);
                }
            }
            if !seen_a_bit {
                return Err(Error::BadTorrentFileNoName);
            }
        }
        if seen_files == 0 {
            return Err(Error::BadTorrentNoFiles);
        }

        // Padding files (BEP 47 "p" attr; the BEP 52 v1-compat layer names
        // them ".pad/<length>") are never materialized on disk - every
        // storage backend skips them (filesystem storage doesn't create
        // them, file_ops reads/writes ignore them) - and their conventional
        // name is just the pad length, so two equal-length pads legitimately
        // share a path. Exempt padding from the uniqueness check so hybrid
        // v1+v2 torrents with repeated pad lengths validate; duplicate
        // non-padding filenames still error (they would genuinely share one
        // on-disk file).
        let mut unique_filenames = HashSet::<PathBuf>::new();
        let mut unique_checkable = 0usize;
        for fd in validated.iter_file_details() {
            if fd.attrs().padding {
                continue;
            }
            unique_checkable += 1;
            let pb = fd.filename.to_pathbuf();
            if pb.as_os_str().is_empty() {
                return Err(Error::BadTorrentFileNoName);
            }
            unique_filenames.insert(pb);
        }
        if unique_filenames.len() != unique_checkable {
            return Err(Error::BadTorrentDuplicateFilenames);
        }

        Ok(validated)
    }

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

    pub fn detect_encoding(&self) -> &'static Encoding {
        let mut encdetect = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
        if let Some(name) = self.name.as_ref() {
            encdetect.feed(name.as_ref(), false);
        }

        for file in self.files.iter().flat_map(|f| f.iter()) {
            for component in file.path.iter() {
                encdetect.feed(component.as_ref(), false);
            }
        }

        encdetect.guess(None, chardetng::Utf8Detection::Allow)
    }

    pub(crate) fn iter_file_details_raw(
        &self,
        encoding: &'static Encoding,
    ) -> crate::Result<impl Iterator<Item = FileDetails<'_, BufType>>> {
        match (self.length, self.files.as_ref()) {
            // Single-file
            (Some(length), None) => Ok(Either::Left(once(FileDetails {
                filename: FileIteratorName {
                    encoding,
                    data: FileIteratorNameData::Single(self.name.as_ref()),
                },
                len: length,
                attr: self.attr.as_ref(),
                sha1: self.sha1.as_ref(),
                symlink_path: self.symlink_path.as_deref(),
            }))),

            // Multi-file
            (None, Some(files)) => {
                if files.is_empty() {
                    return Err(Error::BadTorrentMultiFileEmpty);
                }
                Ok(Either::Right(files.iter().map(move |f| FileDetails {
                    filename: FileIteratorName {
                        encoding,
                        data: FileIteratorNameData::Tree(&f.path),
                    },
                    len: f.length,
                    attr: f.attr.as_ref(),
                    sha1: f.sha1.as_ref(),
                    symlink_path: f.symlink_path.as_deref(),
                })))
            }
            _ => Err(Error::BadTorrentBothSingleAndMultiFile),
        }
    }
}

const fn none<T>() -> Option<T> {
    None
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct TorrentMetaV1File<BufType> {
    pub length: u64,
    pub path: Vec<BufType>,

    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub attr: Option<BufType>,
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub sha1: Option<BufType>,
    #[serde(
        default = "none",
        rename = "symlink path",
        skip_serializing_if = "Option::is_none"
    )]
    pub symlink_path: Option<Vec<BufType>>,
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
            attr: self.attr.clone_to_owned(within_buffer),
            sha1: self.sha1.clone_to_owned(within_buffer),
            symlink_path: self.symlink_path.clone_to_owned(within_buffer),
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
            attr: self.attr.clone_to_owned(within_buffer),
            sha1: self.sha1.clone_to_owned(within_buffer),
            symlink_path: self.symlink_path.clone_to_owned(within_buffer),
            private: self.private,
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
    use bencode::{BencodeValue, from_bytes};

    use super::*;

    const TORRENT_BYTES: &[u8] =
        include_bytes!("../../librqbit/resources/ubuntu-21.04-desktop-amd64.iso.torrent");

    #[test]
    fn test_deserialize_torrent_borrowed() {
        let torrent: TorrentMetaV1Borrowed = from_bytes(TORRENT_BYTES).unwrap();
        dbg!(torrent);
    }

    #[test]
    #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
    fn test_deserialize_torrent_with_info_hash() {
        let torrent: TorrentMetaV1Borrowed = torrent_from_bytes(TORRENT_BYTES).unwrap();
        assert_eq!(
            torrent.info_hash.as_string(),
            "64a980abe6e448226bb930ba061592e44c3781a1"
        );
    }

    #[test]
    fn test_serialize_then_deserialize_bencode() {
        let torrent = from_bytes::<TorrentMetaV1<ByteBuf>>(TORRENT_BYTES)
            .unwrap()
            .info
            .data;
        let mut writer = Vec::new();
        bencode::bencode_serialize_to_writer(&torrent, &mut writer).unwrap();
        let deserialized = from_bytes::<TorrentMetaV1Info<ByteBuf>>(&writer).unwrap();

        assert_eq!(torrent, deserialized);
    }

    #[test]
    fn test_private_serialize_deserialize() {
        for private in [false, true] {
            let info: TorrentMetaV1Info<ByteBufOwned> = TorrentMetaV1Info {
                private,
                ..Default::default()
            };
            let mut buf = Vec::new();
            bencode::bencode_serialize_to_writer(&info, &mut buf).unwrap();

            let deserialized = from_bytes::<TorrentMetaV1Info<ByteBuf>>(&buf).unwrap();
            assert_eq!(info.private, deserialized.private);

            let deserialized_dyn = ::bencode::dyn_from_bytes::<ByteBuf>(&buf).unwrap();
            let hm = match deserialized_dyn {
                bencode::BencodeValue::Dict(hm) => hm,
                _ => panic!("expected dict"),
            };
            match (private, hm.get(&ByteBuf(b"private"))) {
                (true, Some(BencodeValue::Integer(1))) => {}
                (false, None) => {}
                (_, v) => {
                    panic!("unexpected value for \"private\": {v:?}")
                }
            }
        }
    }

    #[test]
    #[cfg(any(feature = "sha1-ring", feature = "sha1-crypto-hash"))]
    fn test_private_real_torrent() {
        let buf = include_bytes!("resources/test/private.torrent");
        let torrent: TorrentMetaV1Borrowed = from_bytes(buf).unwrap();
        assert!(torrent.info.data.private);
    }

    // BEP 47 padding files ("p" attr) are synthetic and never written to
    // disk, and their recommended path is [".pad", "<length>"], so two
    // equal-length pads legitimately share a name. Hybrid v1+v2 (BEP 52)
    // torrents rely on this: the v1 compat layer repeats ".pad/<length>"
    // whenever two files need the same alignment padding.
    #[test]
    fn test_validate_duplicate_pad_paths() {
        let bytes = harness_padded_multi(
            "hybrid",
            &[("a.bin", 1000), ("b.bin", 1000)],
            &[("42", 42), ("42", 42)],
        );
        let torrent = from_bytes::<TorrentMetaV1Borrowed>(&bytes).unwrap();
        let validated = torrent.info.data.validate().unwrap();

        // Pads still count as file entries, but only the real files make it
        // to disk / into "unique" accounting.
        let details: Vec<_> = validated.iter_file_details().collect();
        assert_eq!(details.len(), 4);
        assert_eq!(details.iter().filter(|d| !d.attrs().padding).count(), 2);
        assert_eq!(validated.lengths().total_length(), 2084);
    }

    #[test]
    fn test_validate_duplicate_real_filenames() {
        let bytes = harness_multi_named("dup", &[("same.bin", 10), ("same.bin", 10)]);
        let torrent = from_bytes::<TorrentMetaV1Borrowed>(&bytes).unwrap();
        let err = torrent.info.data.validate().unwrap_err();
        assert!(
            matches!(err, Error::BadTorrentDuplicateFilenames),
            "duplicate real filenames must still be rejected, got {err:?}"
        );
    }

    fn bstr(b: &[u8]) -> Vec<u8> {
        let mut o = format!("{}:", b.len()).into_bytes();
        o.extend_from_slice(b);
        o
    }

    fn bint(v: i64) -> Vec<u8> {
        format!("i{v}e").into_bytes()
    }

    fn bdict(entries: &[(&[u8], Vec<u8>)]) -> Vec<u8> {
        let mut o = vec![b'd'];
        for (k, v) in entries {
            o.extend_from_slice(&bstr(k));
            o.extend_from_slice(v);
        }
        o.push(b'e');
        o
    }

    fn harness_multi_named(name: &str, files: &[(&str, u64)]) -> Vec<u8> {
        let mut flist = vec![b'l'];
        for (fname, len) in files {
            let mut path = vec![b'l'];
            path.extend_from_slice(&bstr(fname.as_bytes()));
            path.push(b'e');
            flist.extend_from_slice(&bdict(&[
                (b"length".as_slice(), bint(*len as i64)),
                (b"path".as_slice(), path),
            ]));
        }
        flist.push(b'e');
        harness_multi_with_files(name, flist)
    }

    // Multi-file metainfo with BEP-47 padding entries: each pad is
    // ".pad/<len>" with attr "p".
    fn harness_padded_multi(name: &str, files: &[(&str, u64)], pads: &[(&str, u64)]) -> Vec<u8> {
        let mut flist = vec![b'l'];
        let push = |flist: &mut Vec<u8>, fname: &[&[u8]], len: i64, attr: Option<&[u8]>| {
            let mut path = vec![b'l'];
            for bit in fname {
                path.extend_from_slice(&bstr(bit));
            }
            path.push(b'e');
            let mut entries: Vec<(&[u8], Vec<u8>)> = vec![
                (b"length".as_slice(), bint(len)),
                (b"path".as_slice(), path),
            ];
            if let Some(a) = attr {
                entries.push((b"attr".as_slice(), bstr(a)));
            }
            flist.extend_from_slice(&bdict(&entries));
        };
        for (fname, len) in files {
            push(&mut flist, &[fname.as_bytes()], *len as i64, None);
        }
        for (fname, len) in pads {
            push(
                &mut flist,
                &[b".pad", fname.as_bytes()],
                *len as i64,
                Some(b"p"),
            );
        }
        flist.push(b'e');
        harness_multi_with_files(name, flist)
    }

    fn harness_multi_with_files(name: &str, flist: Vec<u8>) -> Vec<u8> {
        let pieces = vec![0u8; 20];
        let info = bdict(&[
            (b"files".as_slice(), flist),
            (b"name".as_slice(), bstr(name.as_bytes())),
            (b"piece length".as_slice(), bint(16384)),
            (b"pieces".as_slice(), bstr(&pieces)),
        ]);
        let mut top = vec![b'd'];
        top.extend_from_slice(&bstr(b"announce"));
        top.extend_from_slice(&bstr(b"http"));
        top.extend_from_slice(&bstr(b"info"));
        top.extend_from_slice(&info);
        top.push(b'e');
        top
    }
}
