#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("expected multi-file torrent to have at least one file")]
    BadTorrentMultiFileEmpty,
    #[error("torrent has a file with no name")]
    BadTorrentFileNoName,
    #[error("torrent can't be both in single and multi-file mode")]
    BadTorrentBothSingleAndMultiFile,
    #[error("path traversal detected, \"..\" in filename")]
    BadTorrentPathTraversal,
    #[error("suspicious separator in filename")]
    BadTorrentSeparatorInName,
    #[error("torrent with 0 length is useless")]
    BadTorrentZeroLength,
    #[error("invalid piece index {0}")]
    InvalidPieceIndex(u32),
    #[error("no files in torrent")]
    BadTorrentNoFiles,
    #[error("duplicate filenames in torrent")]
    BadTorrentDuplicateFilenames,
    #[error("empty filename in torrent")]
    BadTorrentEmptyFilename,
}
