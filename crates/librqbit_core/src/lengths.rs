use std::sync::Arc;

use crate::{Error, constants::CHUNK_SIZE, metainfo_v2::TorrentVersion};

pub fn last_element_size<T>(total_length: T, piece_length: T) -> T
where
    T: std::ops::Rem<Output = T> + Default + Eq + Copy,
{
    let rem = total_length % piece_length;
    if rem == T::default() {
        return piece_length;
    }
    rem
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PieceInfo {
    pub piece_index: ValidPieceIndex,
    pub len: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkInfo {
    pub piece_index: ValidPieceIndex,

    // Index of chunk within the piece.
    pub chunk_index: u32,

    // Absolute chunk index if the first chunk of the first piece was 0.
    pub absolute_index: u32,
    pub size: u32,

    // Offset of chunk in bytes within the piece.
    pub offset: u32,
}

/// Piece geometry of a torrent.
///
/// Pieces live in one byte space ("torrent offsets"): piece `p` starts at
/// `p * piece_length`, and the absolute indices of its chunks start at
/// `p * chunks_per_piece`.
///
/// v1 and hybrid torrents are one contiguous stream: every piece is
/// `piece_length` long except the last one, and hybrid torrents express the
/// BEP 52 file alignment with explicit BEP 47 padding files.
///
/// v2-only torrents (BEP 52) give every file its own pieces: a non-empty file
/// starts on a piece boundary and its last piece ends with the file. The bytes
/// between the end of such a short piece and the next piece boundary do not
/// exist (nothing is stored, sent, hashed or counted there), and the absolute
/// chunk indices of a short piece end early, leaving unused indices. Empty
/// files take no piece.
#[derive(Debug, Clone)]
pub struct Lengths {
    // Sum of the lengths of all pieces, i.e. the bytes that exist.
    total_length: u64,

    // One past the last byte, in torrent offsets. Equals total_length unless
    // there are short pieces.
    end_offset: u64,

    // The length in bytes of each piece (except the short ones).
    piece_length: u32,

    // The id and length of the last piece (which may be truncated).
    last_piece_id: u32,
    last_piece_length: u32,

    // How many chunks are there per normal piece (except the short ones).
    chunks_per_piece: u32,

    // v2-only torrents: (piece index, length) of every short piece other than
    // the last piece of the torrent, sorted by piece index.
    short_pieces: Option<Arc<[(u32, u32)]>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ValidPieceIndex(u32);
impl std::fmt::Display for ValidPieceIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::fmt::Debug for ValidPieceIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl ValidPieceIndex {
    pub const fn get(&self) -> u32 {
        self.0
    }
    pub const fn get_usize(&self) -> usize {
        self.0 as usize
    }
}

impl Lengths {
    pub fn from_torrent<ByteBuf: AsRef<[u8]>>(
        torrent: &crate::torrent_metainfo::TorrentMetaV1Info<ByteBuf>,
    ) -> crate::Result<Lengths> {
        if torrent.version()? == TorrentVersion::V2 {
            let tree = torrent.file_tree.as_ref().ok_or(Error::V2MissingFileTree)?;
            return Lengths::new_aligned(tree.files.iter().map(|f| f.length), torrent.piece_length);
        }
        let mut total_length = 0u64;
        for d in torrent.iter_file_details_raw(encoding_rs::UTF_8)? {
            total_length = total_length
                .checked_add(d.len)
                .ok_or(Error::BadTorrentTooManyPieces)?;
        }
        Lengths::new(total_length, torrent.piece_length)
    }

    /// Geometry of one contiguous stream of `total_length` bytes (v1 and
    /// hybrid torrents).
    pub fn new(total_length: u64, piece_length: u32) -> crate::Result<Self> {
        if total_length == 0 {
            return Err(Error::BadTorrentZeroLength);
        }
        if piece_length == 0 {
            return Err(Error::BadTorrentZeroPieceLength);
        }
        let total_pieces = total_length.div_ceil(piece_length as u64);
        Self::build(
            piece_length,
            total_length,
            total_pieces,
            last_element_size(total_length, piece_length as u64) as u32,
            None,
        )
    }

    /// Geometry of a v2-only torrent (BEP 52): every non-empty file starts on
    /// a piece boundary and its last piece ends with the file. Empty files take
    /// no piece. `file_lengths` are in torrent order.
    pub fn new_aligned(
        file_lengths: impl IntoIterator<Item = u64>,
        piece_length: u32,
    ) -> crate::Result<Self> {
        if piece_length == 0 {
            return Err(Error::V2InvalidPieceLength(0));
        }
        let pl = piece_length as u64;
        let mut total_length = 0u64;
        let mut total_pieces = 0u64;
        let mut last_piece_length = piece_length;
        let mut short = Vec::new();
        for len in file_lengths {
            if len == 0 {
                continue;
            }
            total_length = total_length
                .checked_add(len)
                .ok_or(Error::BadTorrentTooManyPieces)?;
            total_pieces = total_pieces
                .checked_add(len.div_ceil(pl))
                .ok_or(Error::BadTorrentTooManyPieces)?;
            last_piece_length = last_element_size(len, pl) as u32;
            if last_piece_length != piece_length {
                let id =
                    u32::try_from(total_pieces - 1).map_err(|_| Error::BadTorrentTooManyPieces)?;
                short.push((id, last_piece_length));
            }
        }
        if total_length == 0 {
            return Err(Error::BadTorrentZeroLength);
        }
        // The last piece of the torrent is described by last_piece_length.
        if short.last().map(|(id, _)| u64::from(*id) + 1) == Some(total_pieces) {
            short.pop();
        }
        let short = (!short.is_empty()).then(|| Arc::from(short));
        Self::build(
            piece_length,
            total_length,
            total_pieces,
            last_piece_length,
            short,
        )
    }

    fn build(
        piece_length: u32,
        total_length: u64,
        total_pieces: u64,
        last_piece_length: u32,
        short_pieces: Option<Arc<[(u32, u32)]>>,
    ) -> crate::Result<Self> {
        let chunks_per_piece = piece_length.div_ceil(CHUNK_SIZE);
        // Absolute chunk indices are u32: every piece index times
        // chunks_per_piece plus the chunks of the last piece must fit.
        if total_pieces == 0
            || total_pieces
                .checked_mul(chunks_per_piece as u64)
                .is_none_or(|c| c > u32::MAX as u64)
        {
            return Err(Error::BadTorrentTooManyPieces);
        }
        let last_piece_id = (total_pieces - 1) as u32;
        Ok(Self {
            total_length,
            end_offset: last_piece_id as u64 * piece_length as u64 + last_piece_length as u64,
            piece_length,
            last_piece_id,
            last_piece_length,
            chunks_per_piece,
            short_pieces,
        })
    }

    // How many bytes are required to store a bitfield where there's one bit for each piece.
    pub const fn piece_bitfield_bytes(&self) -> usize {
        self.total_pieces().div_ceil(8) as usize
    }

    // How many bytes are required to store a bitfield where there's one bit for each chunk.
    pub const fn chunk_bitfield_bytes(&self) -> usize {
        self.total_chunks().div_ceil(8) as usize
    }

    /// How many bytes of data the torrent has (padding files of hybrid
    /// torrents included, the gaps after short pieces of v2 torrents not).
    pub const fn total_length(&self) -> u64 {
        self.total_length
    }

    /// One past the last byte of the last piece, in torrent offsets. Larger
    /// than [`Self::total_length`] when the torrent has short pieces.
    pub const fn end_offset(&self) -> u64 {
        self.end_offset
    }

    /// True if pieces other than the last one can be shorter than the piece
    /// length (v2-only torrents with more than one file).
    pub fn has_short_pieces(&self) -> bool {
        self.short_pieces.is_some()
    }

    pub const fn validate_piece_index(&self, index: u32) -> Option<ValidPieceIndex> {
        if index > self.last_piece_id {
            return None;
        }
        Some(ValidPieceIndex(index))
    }
    pub fn try_validate_piece_index(&self, index: u32) -> crate::Result<ValidPieceIndex> {
        self.validate_piece_index(index)
            .ok_or(Error::InvalidPieceIndex(index))
    }
    pub const fn default_piece_length(&self) -> u32 {
        self.piece_length
    }
    pub const fn default_chunks_per_piece(&self) -> u32 {
        self.chunks_per_piece
    }

    /// One past the largest absolute chunk index. With short pieces some
    /// indices below it belong to no chunk.
    pub const fn total_chunks(&self) -> u32 {
        self.last_piece_id * self.default_chunks_per_piece()
            + self.last_piece_length.div_ceil(CHUNK_SIZE)
    }
    pub const fn last_piece_id(&self) -> ValidPieceIndex {
        ValidPieceIndex(self.last_piece_id)
    }
    pub const fn total_pieces(&self) -> u32 {
        self.last_piece_id + 1
    }
    pub fn piece_length(&self, index: ValidPieceIndex) -> u32 {
        if index.0 == self.last_piece_id {
            return self.last_piece_length;
        }
        if let Some(short) = self.short_pieces.as_deref()
            && let Ok(pos) = short.binary_search_by_key(&index.0, |(id, _)| *id)
            && let Some((_, len)) = short.get(pos)
        {
            return *len;
        }
        self.piece_length
    }
    pub const fn chunk_absolute_offset(&self, chunk_info: &ChunkInfo) -> u64 {
        self.piece_offset(chunk_info.piece_index) + chunk_info.offset as u64
    }
    pub const fn piece_offset(&self, index: ValidPieceIndex) -> u64 {
        index.0 as u64 * self.piece_length as u64
    }

    pub fn iter_piece_infos(&self) -> impl Iterator<Item = PieceInfo> + 'static {
        let this = self.clone();
        (0..self.total_pieces()).map(move |idx| {
            let piece_index = ValidPieceIndex(idx);
            PieceInfo {
                piece_index,
                len: this.piece_length(piece_index),
            }
        })
    }

    // A helper to iterate over pieces in a file.
    pub fn iter_pieces_within_offset(&self, offset_bytes: u64, len: u64) -> std::ops::Range<u32> {
        // Validation and correction
        let offset_bytes = offset_bytes.min(self.end_offset);
        let end_bytes = offset_bytes.saturating_add(len).min(self.end_offset);

        let start_piece_id = (offset_bytes / self.piece_length as u64) as u32;
        let end_piece_id = if end_bytes == offset_bytes {
            start_piece_id
        } else {
            end_bytes.div_ceil(self.piece_length as u64) as u32
        };
        start_piece_id..end_piece_id
    }

    pub fn iter_chunk_infos(&self, index: ValidPieceIndex) -> impl Iterator<Item = ChunkInfo> {
        let mut remaining = self.piece_length(index);
        let absolute_offset = index.0 * self.chunks_per_piece;
        (0u32..).scan(0, move |offset, idx| {
            if remaining == 0 {
                return None;
            }
            let s = std::cmp::min(remaining, CHUNK_SIZE);
            let result = ChunkInfo {
                piece_index: index,
                chunk_index: idx,
                absolute_index: absolute_offset + idx,
                size: s,
                offset: *offset,
            };
            *offset += s;
            remaining -= s;
            Some(result)
        })
    }

    pub fn chunk_info_from_received_data(
        &self,
        piece_index: ValidPieceIndex,
        begin: u32,
        chunk_size: u32,
    ) -> Option<ChunkInfo> {
        let index = begin / CHUNK_SIZE;
        let expected_chunk_size = self.chunk_size(piece_index, index)?;
        let offset = self.chunk_offset_in_piece(piece_index, index)?;
        if offset != begin {
            return None;
        }
        if expected_chunk_size != chunk_size {
            return None;
        }
        let absolute_index = self.chunks_per_piece * piece_index.get() + index;
        Some(ChunkInfo {
            piece_index,
            chunk_index: index,
            size: chunk_size,
            offset,
            absolute_index,
        })
    }
    pub fn chunk_range(&self, index: ValidPieceIndex) -> std::ops::Range<usize> {
        let start = index.0 * self.chunks_per_piece;
        let end = start + self.chunks_per_piece(index);
        start as usize..end as usize
    }
    pub fn chunks_per_piece(&self, index: ValidPieceIndex) -> u32 {
        self.piece_length(index).div_ceil(CHUNK_SIZE)
    }
    pub fn chunk_offset_in_piece(
        &self,
        piece_index: ValidPieceIndex,
        chunk_index: u32,
    ) -> Option<u32> {
        if chunk_index >= self.chunks_per_piece(piece_index) {
            return None;
        }
        Some(chunk_index * CHUNK_SIZE)
    }
    pub fn chunk_size(&self, piece_index: ValidPieceIndex, chunk_index: u32) -> Option<u32> {
        let piece_length = self.piece_length(piece_index);
        let last_chunk_id = piece_length.div_ceil(CHUNK_SIZE).checked_sub(1)?;
        if chunk_index < last_chunk_id {
            return Some(CHUNK_SIZE);
        }
        if chunk_index == last_chunk_id {
            return Some(last_element_size(piece_length, CHUNK_SIZE));
        }
        None
    }

    // How many bytes out of the given piece are present in the given file (by offset and len).
    pub fn size_of_piece_in_file(&self, piece_id: u32, file_offset: u64, file_len: u64) -> u64 {
        let piece_offset = piece_id as u64 * self.default_piece_length() as u64;
        // Files never extend into the gap after a short piece, so the
        // default piece length bounds the overlap correctly for them too.
        let piece_end = piece_offset + self.default_piece_length() as u64;

        let file_end = file_offset + file_len;

        let offset = file_offset.max(piece_offset);
        let end = file_end.min(piece_end);

        end.saturating_sub(offset)
    }

    pub fn compute_current_piece(
        self: &Lengths,
        file_pos: u64,
        file_torrent_abs_offset: u64,
    ) -> Option<CurrentPiece> {
        let dpl = self.default_piece_length();

        let abs_pos = file_torrent_abs_offset.checked_add(file_pos)?;
        let piece_id = abs_pos / dpl as u64;
        let piece_id: u32 = piece_id.try_into().ok()?;

        let piece_id = self.validate_piece_index(piece_id)?;
        let piece_len = self.piece_length(piece_id);
        // Offset *within* the piece, not the piece number. abs_pos / dpl is
        // already `piece_id`, so dividing here made piece_offset equal the
        // piece index, which is out of range for the piece buffer as soon as
        // the piece index is >= 1.
        let piece_offset = abs_pos % dpl as u64;
        Some(CurrentPiece {
            id: piece_id,
            piece_offset: piece_offset.try_into().ok()?,
            // None past the end of a short piece: that byte does not exist.
            piece_remaining: (piece_len as u64)
                .checked_sub(piece_offset)?
                .try_into()
                .ok()?,
        })
    }
}

pub struct CurrentPiece {
    pub id: ValidPieceIndex,
    pub piece_remaining: u32,
    pub piece_offset: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_lengths() -> Lengths {
        Lengths::new(1174243328, 262144).unwrap()
    }

    #[test]
    fn test_total_pieces() {
        let l = make_lengths();
        assert_eq!(l.total_pieces(), 4480);
    }

    #[test]
    fn test_total_pieces_2() {
        let l = Lengths::new(4148166656, 2097152).unwrap();
        assert_eq!(l.total_pieces(), 1978);
    }

    #[test]
    fn test_piece_length() {
        let l = make_lengths();
        let p = l.validate_piece_index(4479).unwrap();

        assert_eq!(l.piece_length(l.validate_piece_index(0).unwrap()), 262144);
        assert_eq!(l.piece_length(p), 100352);
    }

    #[test]
    fn test_chunks_in_piece() {
        let l = make_lengths();
        let p = l.validate_piece_index(4479).unwrap();

        assert_eq!(l.chunks_per_piece(l.validate_piece_index(0).unwrap()), 16);
        assert_eq!(l.chunks_per_piece(p), 7);
    }

    #[test]
    fn test_chunk_size() {
        let l = make_lengths();
        let p = l.validate_piece_index(4479).unwrap();

        assert_eq!(l.chunk_size(p, 0), Some(16384));
        assert_eq!(l.chunk_size(p, 6), Some(2048));
    }

    #[test]
    fn test_chunk_infos() {
        let l = make_lengths();
        let p = l.validate_piece_index(4479).unwrap();

        let mut it = l.iter_chunk_infos(p);
        let first = it.next().unwrap();
        let last = it.last().unwrap();

        assert_eq!(
            first,
            ChunkInfo {
                piece_index: p,
                chunk_index: 0,
                absolute_index: 71664,
                size: 16384,
                offset: 0,
            }
        );

        assert_eq!(
            last,
            ChunkInfo {
                piece_index: p,
                chunk_index: 6,
                absolute_index: 71670,
                size: 2048,
                offset: 98304,
            }
        );
    }

    #[test]
    fn test_lengths_extensive() {
        for (
            total_is_divisible_no_remainder,
            piece_is_chunk_multiple,
            more_than_one_piece,
            more_than_one_chunk_per_piece,
        ) in [
            (true, true, true, true),
            (true, true, true, false),
            (true, true, false, true),
            (true, true, false, false),
            (true, false, true, true),
            (true, false, true, false),
            (true, false, false, true),
            (true, false, false, false),
            (false, true, true, true),
            (false, true, true, false),
            (false, true, false, true),
            (false, true, false, false),
            (false, false, true, true),
            (false, false, true, false),
            (false, false, false, true),
            (false, false, false, false),
        ] {
            let a = total_is_divisible_no_remainder;
            let b = piece_is_chunk_multiple;
            let c = more_than_one_piece;
            let d = more_than_one_chunk_per_piece;

            let check = |l: Lengths| -> Lengths {
                if a {
                    assert_eq!(l.total_length() % l.default_piece_length() as u64, 0);
                } else {
                    assert!(
                        !l.total_length()
                            .is_multiple_of(l.default_piece_length() as u64)
                    );
                }
                if b {
                    assert_eq!(l.default_piece_length() % CHUNK_SIZE, 0)
                } else {
                    assert!(!l.default_piece_length().is_multiple_of(CHUNK_SIZE))
                }
                if c {
                    assert!(l.total_length().div_ceil(l.default_piece_length() as u64) > 1);
                } else {
                    assert_eq!(
                        l.total_length().div_ceil(l.default_piece_length() as u64),
                        1
                    );
                }
                if d {
                    assert!(l.default_piece_length().div_ceil(CHUNK_SIZE) > 1);
                } else {
                    assert_eq!(l.default_piece_length().div_ceil(CHUNK_SIZE), 1);
                }
                l
            };

            macro_rules! i {
                ($n:tt) => {
                    ValidPieceIndex($n)
                };
            }

            match (a, b, c, d) {
                // (true, true, ___)
                (true, true, true, true) => {
                    let l = check(Lengths::new(65536, 32768).unwrap());
                    assert_eq!(l.total_pieces(), 2);
                    assert_eq!(l.total_chunks(), 4);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 2);
                    assert_eq!(l.chunk_size(i!(1), 0), Some(CHUNK_SIZE));
                    assert_eq!(l.chunk_size(i!(1), 1), Some(CHUNK_SIZE));
                    assert_eq!(l.chunk_size(i!(1), 2), None);
                }
                (true, true, true, false) => {
                    let l = check(Lengths::new(32768, 16384).unwrap());
                    assert_eq!(l.total_pieces(), 2);
                    assert_eq!(l.total_chunks(), 2);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 1);
                    assert_eq!(l.chunk_size(i!(1), 0), Some(CHUNK_SIZE));
                    assert_eq!(l.chunk_size(i!(1), 1), None);
                }
                (true, true, false, true) => {
                    let l = check(Lengths::new(32768, 32768).unwrap());
                    dbg!(l.total_length().div_ceil(l.default_piece_length() as u64));
                    assert_eq!(l.total_pieces(), 1);
                    assert_eq!(l.total_chunks(), 2);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 2);
                    assert_eq!(l.chunk_size(i!(0), 0), Some(CHUNK_SIZE));
                    assert_eq!(l.chunk_size(i!(0), 1), Some(CHUNK_SIZE));
                    assert_eq!(l.chunk_size(i!(0), 2), None);
                }
                (true, true, false, false) => {
                    let l = check(Lengths::new(16384, 16384).unwrap());
                    assert_eq!(l.total_pieces(), 1);
                    assert_eq!(l.total_chunks(), 1);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 1);
                    assert_eq!(l.chunk_size(i!(0), 0), Some(CHUNK_SIZE));
                    assert_eq!(l.chunk_size(i!(0), 1), None);
                }

                // (true, false, ___)
                (true, false, true, true) => {
                    let l = check(Lengths::new(40000, 20000).unwrap());
                    assert_eq!(l.total_pieces(), 2);
                    assert_eq!(l.total_chunks(), 4);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 2);
                    assert_eq!(l.chunk_size(i!(1), 0), Some(CHUNK_SIZE));
                    assert_eq!(l.chunk_size(i!(1), 1), Some(20000 - CHUNK_SIZE));
                    assert_eq!(l.chunk_size(i!(1), 2), None);
                }
                (true, false, true, false) => {
                    let l = check(Lengths::new(20000, 10000).unwrap());
                    assert_eq!(l.total_pieces(), 2);
                    assert_eq!(l.total_chunks(), 2);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 1);
                    assert_eq!(l.chunk_size(i!(1), 0), Some(10000));
                    assert_eq!(l.chunk_size(i!(1), 1), None);
                }
                (true, false, false, true) => {
                    let l = check(Lengths::new(20000, 20000).unwrap());
                    assert_eq!(l.total_pieces(), 1);
                    assert_eq!(l.total_chunks(), 2);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 2);
                    assert_eq!(l.chunk_size(i!(0), 0), Some(CHUNK_SIZE));
                    assert_eq!(l.chunk_size(i!(0), 1), Some(20000 - CHUNK_SIZE));
                    assert_eq!(l.chunk_size(i!(0), 2), None);
                }
                (true, false, false, false) => {
                    let l = check(Lengths::new(10000, 10000).unwrap());
                    assert_eq!(l.total_pieces(), 1);
                    assert_eq!(l.total_chunks(), 1);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 1);
                    assert_eq!(l.chunk_size(i!(0), 0), Some(10000));
                    assert_eq!(l.chunk_size(i!(0), 1), None);
                }

                // (false, true, ___)
                (false, true, true, true) => {
                    let l = check(Lengths::new(35000, 32768).unwrap());
                    assert_eq!(l.total_pieces(), 2);
                    assert_eq!(l.total_chunks(), 3);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 1);
                    assert_eq!(l.chunk_size(i!(1), 0), Some(35000 - 32768));
                    assert_eq!(l.chunk_size(i!(1), 1), None);
                }
                (false, true, true, false) => {
                    let l = check(Lengths::new(20000, 16384).unwrap());
                    assert_eq!(l.total_pieces(), 2);
                    assert_eq!(l.total_chunks(), 2);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 1);
                    assert_eq!(l.chunk_size(i!(1), 0), Some(20000 - 16384));
                    assert_eq!(l.chunk_size(i!(1), 1), None);
                }
                (false, true, false, true) => {
                    let l = check(Lengths::new(20000, 32768).unwrap());
                    assert_eq!(l.total_pieces(), 1);
                    assert_eq!(l.total_chunks(), 2);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 2);
                    assert_eq!(l.chunk_size(i!(0), 0), Some(CHUNK_SIZE));
                    assert_eq!(l.chunk_size(i!(0), 1), Some(20000 - CHUNK_SIZE));
                    assert_eq!(l.chunk_size(i!(0), 2), None);
                }
                (false, true, false, false) => {
                    let l = check(Lengths::new(15000, 16384).unwrap());
                    assert_eq!(l.total_pieces(), 1);
                    assert_eq!(l.total_chunks(), 1);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 1);
                    assert_eq!(l.chunk_size(i!(0), 0), Some(15000));
                    assert_eq!(l.chunk_size(i!(0), 1), None);
                }

                // (false, false, ___)
                (false, false, true, true) => {
                    let l = check(Lengths::new(21000, 20000).unwrap());
                    assert_eq!(l.total_pieces(), 2);
                    assert_eq!(l.total_chunks(), 3);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 1);
                    assert_eq!(l.chunk_size(i!(0), 0), Some(CHUNK_SIZE));
                    assert_eq!(l.chunk_size(i!(0), 1), Some(20000 - CHUNK_SIZE));
                    assert_eq!(l.chunk_size(i!(0), 2), None);
                    assert_eq!(l.chunk_size(i!(1), 0), Some(1000));
                    assert_eq!(l.chunk_size(i!(1), 1), None);
                }
                (false, false, true, false) => {
                    let l = check(Lengths::new(21000, 10000).unwrap());
                    assert_eq!(l.total_pieces(), 3);
                    assert_eq!(l.total_chunks(), 3);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 1);
                    assert_eq!(l.chunk_size(i!(1), 0), Some(10000));
                    assert_eq!(l.chunk_size(i!(1), 1), None);
                    assert_eq!(l.chunk_size(i!(2), 0), Some(1000));
                    assert_eq!(l.chunk_size(i!(2), 1), None);
                }
                (false, false, false, true) => {
                    let l = check(Lengths::new(11000, 20000).unwrap());
                    assert_eq!(l.total_pieces(), 1);
                    assert_eq!(l.total_chunks(), 1);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 1);
                    assert_eq!(l.chunk_size(i!(0), 0), Some(11000));
                    assert_eq!(l.chunk_size(i!(0), 1), None);
                }
                (false, false, false, false) => {
                    let l = check(Lengths::new(9000, 10000).unwrap());
                    assert_eq!(l.total_pieces(), 1);
                    assert_eq!(l.total_chunks(), 1);
                    assert_eq!(l.chunks_per_piece(l.last_piece_id()), 1);
                    assert_eq!(l.chunk_size(i!(0), 0), Some(9000));
                    assert_eq!(l.chunk_size(i!(0), 1), None);
                }
            }
        }

        // A few more examples with longer values and weird inputs.

        let l = Lengths::new(1_638_411, 163_841).unwrap();
        assert_eq!(l.default_chunks_per_piece(), 11);
        assert_eq!(l.total_pieces(), 11);
        assert_eq!(l.total_chunks(), 111);
        assert_eq!(l.piece_bitfield_bytes(), 2);
        assert_eq!(l.chunk_bitfield_bytes(), 14);

        assert_eq!(l.chunks_per_piece(l.last_piece_id()), 1);
    }

    #[test]
    fn test_iter_pieces_within() {
        // Macro to preserve line numbers
        macro_rules! check {
            ($l:expr, $offset:expr, $len:expr, $expected:expr) => {
                let e: &[u32] = $expected;
                println!("case: offset={}, len={}, expected={:?}", $offset, $len, e);
                assert_eq!(
                    &$l.iter_pieces_within_offset($offset, $len)
                        .collect::<Vec<_>>()[..],
                    e
                );
            };
        }

        let l = Lengths::new(21, 10).unwrap();
        check!(&l, 0, 5, &[0]);
        check!(&l, 0, 10, &[0]);
        check!(&l, 0, 11, &[0, 1]);
        check!(&l, 0, 0, &[]);
        check!(&l, 10, 0, &[]);
        check!(&l, 10, 1, &[1]);
        check!(&l, 10, 10, &[1]);
        check!(&l, 10, 11, &[1, 2]);

        check!(&l, 5, 5, &[0]);
        check!(&l, 5, 6, &[0, 1]);
        check!(&l, 5, 15, &[0, 1]);
        check!(&l, 5, 16, &[0, 1, 2]);

        check!(&l, 20, 1, &[2]);
        check!(&l, 20, 2, &[2]);
        check!(&l, 20, 1000, &[2]);
        check!(&l, 21, 0, &[]);
        check!(&l, 21, 1, &[]);
        check!(&l, 22, 0, &[]);
        check!(&l, 22, 1, &[]);
    }

    #[test]
    fn test_size_of_piece_in_file() {
        let l = Lengths::new(10, 5).unwrap();

        assert_eq!(l.size_of_piece_in_file(0, 0, 10), 5);
        assert_eq!(l.size_of_piece_in_file(0, 1, 10), 4);
        assert_eq!(l.size_of_piece_in_file(0, 5, 10), 0);
        assert_eq!(l.size_of_piece_in_file(0, 6, 10), 0);

        assert_eq!(l.size_of_piece_in_file(0, 0, 0), 0);
        assert_eq!(l.size_of_piece_in_file(0, 1, 0), 0);
        assert_eq!(l.size_of_piece_in_file(0, 5, 0), 0);
        assert_eq!(l.size_of_piece_in_file(0, 6, 0), 0);

        assert_eq!(l.size_of_piece_in_file(1, 0, 10), 5);
        assert_eq!(l.size_of_piece_in_file(1, 4, 10), 5);
        assert_eq!(l.size_of_piece_in_file(1, 5, 10), 5);
        assert_eq!(l.size_of_piece_in_file(1, 6, 10), 4);
        assert_eq!(l.size_of_piece_in_file(1, 9, 10), 1);
        assert_eq!(l.size_of_piece_in_file(1, 10, 10), 0);

        // garbage data
        assert_eq!(l.size_of_piece_in_file(2, 0, 10), 0);
        assert_eq!(l.size_of_piece_in_file(3, 0, 10), 0);
        assert_eq!(l.size_of_piece_in_file(0, 10, 0), 0);
        assert_eq!(l.size_of_piece_in_file(0, 10, 5), 0);
    }

    // CurrentPiece::piece_offset is the offset *within* the piece. It used to be
    // abs_pos / dpl, i.e. the piece number, which is out of range for the piece
    // buffer as soon as the piece index is >= 1 (and wrong for any partial window).
    #[test]
    fn test_compute_current_piece() {
        let l = Lengths::new(10, 5).unwrap(); // 2 pieces of 5 bytes
        let cur = |file_pos, file_abs| l.compute_current_piece(file_pos, file_abs).unwrap();

        assert_eq!(cur(0, 0).piece_offset, 0);
        assert_eq!(cur(4, 0).piece_offset, 4);

        // exactly at the piece boundary -> piece 1, offset 0
        let c = cur(5, 0);
        assert_eq!((c.id.get(), c.piece_offset, c.piece_remaining), (1, 0, 5));

        // last byte of the torrent
        let c = cur(9, 0);
        assert_eq!((c.id.get(), c.piece_offset, c.piece_remaining), (1, 4, 1));

        // past the first piece, with the file's own offset inside the torrent
        let c = cur(1, 8);
        assert_eq!((c.id.get(), c.piece_offset, c.piece_remaining), (1, 4, 1));

        // dpl == 1: every byte is its own piece
        let l = Lengths::new(4, 1).unwrap();
        for i in 0..4u32 {
            let c = l.compute_current_piece(i.into(), 0).unwrap();
            assert_eq!((c.id.get(), c.piece_offset, c.piece_remaining), (i, 0, 1));
        }
    }

    /// Checks every invariant of the v2 (aligned) layout for one list of
    /// file lengths.
    fn check_aligned_layout(files: &[u64], piece_length: u32) {
        let l = Lengths::new_aligned(files.iter().copied(), piece_length).unwrap();
        let pl = piece_length as u64;

        // Where each non-empty file sits in torrent offsets.
        let mut ranges = Vec::new();
        let mut acc = 0u64;
        for &len in files {
            let offset = acc.next_multiple_of(pl);
            acc = offset + len;
            if len > 0 {
                ranges.push((offset, offset + len));
            }
        }
        assert_eq!(l.total_length(), files.iter().sum::<u64>());
        assert_eq!(l.end_offset(), ranges.last().unwrap().1);

        // Pieces: the files' piece ranges are consecutive and cover every
        // piece; a piece's length is the part of its file inside it.
        let mut next_piece = 0u32;
        for &(start, end) in &ranges {
            let pieces = l.iter_pieces_within_offset(start, end - start);
            assert_eq!(pieces.start, next_piece, "files={files:?}");
            assert_eq!(pieces.len() as u64, (end - start).div_ceil(pl));
            next_piece = pieces.end;
            for p in pieces {
                let p = l.validate_piece_index(p).unwrap();
                let off = l.piece_offset(p);
                let expected = (end - off).min(pl);
                assert_eq!(l.piece_length(p) as u64, expected, "files={files:?} p={p}");
                assert_eq!(
                    l.size_of_piece_in_file(p.get(), start, end - start),
                    expected
                );
            }
        }
        assert_eq!(next_piece, l.total_pieces());
        assert_eq!(
            l.iter_piece_infos().map(|p| p.len as u64).sum::<u64>(),
            l.total_length()
        );

        // Chunks: every real byte exactly once, never a byte of a gap, and
        // every chunk can be found again from what a peer would send.
        let mut covered = Vec::new();
        let mut seen_abs = std::collections::HashSet::new();
        for p in l.iter_piece_infos() {
            let chunks: Vec<_> = l.iter_chunk_infos(p.piece_index).collect();
            assert_eq!(chunks.len() as u32, l.chunks_per_piece(p.piece_index));
            let range = l.chunk_range(p.piece_index);
            for c in chunks {
                assert!(range.contains(&(c.absolute_index as usize)));
                assert!(c.absolute_index < l.total_chunks());
                assert!(seen_abs.insert(c.absolute_index));
                assert_eq!(
                    l.chunk_info_from_received_data(p.piece_index, c.offset, c.size),
                    Some(c)
                );
                let start = l.chunk_absolute_offset(&c);
                covered.push((start, start + c.size as u64));
            }
            // Nothing can be requested past the end of a piece.
            let n = l.chunks_per_piece(p.piece_index);
            assert_eq!(l.chunk_size(p.piece_index, n), None);
            assert_eq!(l.chunk_offset_in_piece(p.piece_index, n), None);
        }
        covered.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (s, e) in covered {
            match merged.last_mut() {
                Some(last) if last.1 == s => last.1 = e,
                Some(last) => {
                    assert!(last.1 < s, "overlapping chunks, files={files:?}");
                    merged.push((s, e));
                }
                None => merged.push((s, e)),
            }
        }
        let mut expected: Vec<(u64, u64)> = Vec::new();
        for &(s, e) in &ranges {
            match expected.last_mut() {
                Some(last) if last.1 == s => last.1 = e,
                _ => expected.push((s, e)),
            }
        }
        assert_eq!(merged, expected, "files={files:?}");

        // Positions inside a file map to its pieces and back.
        for &(start, end) in &ranges {
            for pos in [0, 1, (end - start) / 2, end - start - 1] {
                let pos = pos.min(end - start - 1);
                let c = l.compute_current_piece(pos, start).unwrap();
                assert_eq!(l.piece_offset(c.id) + c.piece_offset as u64, start + pos);
                assert_eq!(
                    c.piece_offset as u64 + c.piece_remaining as u64,
                    l.piece_length(c.id) as u64
                );
            }
        }
    }

    #[test]
    fn test_aligned_layout_examples() {
        let pl = 65536u32;
        let p = pl as u64;
        check_aligned_layout(&[1], pl);
        check_aligned_layout(&[p], pl);
        check_aligned_layout(&[p, 1, 0, p + 1, 0], pl);
        check_aligned_layout(&[0, 5000, 40000, 2 * p, 300000, 0, 70000, 1100000], pl);

        let l = Lengths::new_aligned([5000, 0, 2 * p + 1, 3], pl).unwrap();
        assert_eq!(l.total_pieces(), 5);
        assert!(l.has_short_pieces());
        let lens: Vec<u32> = l.iter_piece_infos().map(|p| p.len).collect();
        assert_eq!(lens, [5000, pl, pl, 1, 3]);
        // A short piece leaves chunk indices unused.
        assert_eq!(l.chunk_range(l.validate_piece_index(0).unwrap()), 0..1);
        assert_eq!(l.chunk_range(l.validate_piece_index(1).unwrap()), 4..8);
        assert_eq!(l.total_chunks(), 4 * 4 + 1);
        // The bytes after a short piece do not exist (the end of the file
        // itself is a position with nothing left, as for v1 torrents).
        assert_eq!(l.compute_current_piece(5000, 0).unwrap().piece_remaining, 0);
        assert!(l.compute_current_piece(5001, 0).is_none());
    }

    #[test]
    fn test_aligned_layout_rejects_empty_and_bad_input() {
        assert!(Lengths::new_aligned([0, 0], 16384).is_err());
        assert!(Lengths::new_aligned([], 16384).is_err());
        assert!(Lengths::new_aligned([1], 0).is_err());
        // Too many pieces for u32 chunk indices.
        assert!(Lengths::new_aligned([u64::MAX / 2, u64::MAX / 2], 16384).is_err());
    }

    #[test]
    fn test_aligned_layout_random() {
        use rand::{RngExt, SeedableRng, rngs::SmallRng};

        let mut rng = SmallRng::seed_from_u64(0xb7e52);
        for _ in 0..300 {
            let piece_length = 16384u32 << rng.random_range(0..3u32);
            let p = piece_length as u64;
            let n = rng.random_range(1..8usize);
            let mut files: Vec<u64> = (0..n)
                .map(|_| match rng.random_range(0..6u32) {
                    0 => 0,
                    1 => p * rng.random_range(1..4u64),
                    2 => rng.random_range(1..p),
                    3 => p * rng.random_range(1..4u64) + rng.random_range(1..3u64),
                    _ => rng.random_range(1..6 * p),
                })
                .collect();
            if files.iter().all(|f| *f == 0) {
                files.push(1);
            }
            check_aligned_layout(&files, piece_length);
        }
    }
}
