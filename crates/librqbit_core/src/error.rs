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

    // BEP 52 (v2) errors
    #[error("v2 torrent missing file_tree")]
    V2MissingFileTree,
    #[error("v2 torrent file_tree root must be a directory, not a file")]
    V2FileTreeRootIsFile,
    #[error("v2 file_tree has \".\" path component")]
    V2FileTreeDotComponent,
    #[error("v2 torrent missing meta_version")]
    V2MissingMetaVersion,
    #[error("unsupported meta_version: {0}")]
    V2UnsupportedMetaVersion(u32),
    #[error("v2 torrent missing piece_layers")]
    V2MissingPieceLayers,
    #[error("v2 piece_layers missing entry for file: {0}")]
    V2MissingPieceLayersEntry(String),
    #[error("v2 piece_layers entry has wrong size: expected {expected}, got {actual}")]
    V2PieceLayersWrongSize { expected: usize, actual: usize },
    #[error("v2 piece_layers count mismatch: expected {expected}, got {actual}")]
    V2PieceLayerCountMismatch { expected: usize, actual: usize },
    #[error("v2 piece_layers merkle root mismatch for file")]
    V2PieceLayersRootMismatch,
    #[error("v2 small file should not have piece_layers entry")]
    V2SmallFileShouldNotHavePieceLayers,
    #[error("v2 small file missing pieces_root")]
    V2SmallFileMissingPiecesRoot,
    #[error("v2 zero-length file should not have pieces_root")]
    V2ZeroLengthFileHasPiecesRoot,
    #[error("invalid v2 piece length {0}: must be power of two and >= 16384")]
    V2InvalidPieceLength(u32),
    #[error("invalid v2 torrent: has meta_version but no pieces and no file_tree")]
    V2InvalidTorrent,
    #[error("v2 hybrid file list mismatch: {0}")]
    V2HybridFileListMismatch(String),
}
