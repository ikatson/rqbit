use anyhow::Context;

use crate::{constants::CHUNK_SIZE, torrent_metainfo::TorrentMetaV1Info};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy)]
pub struct Lengths {
    // The total length of the torrent in bytes.
    total_length: u64,

    // The length in bytes of each piece (except the last one).
    piece_length: u32,

    // The id and length of the last piece (which may be truncated).
    last_piece_id: u32,
    last_piece_length: u32,

    // How many chunks are there per normal piece (except the last piece).
    chunks_per_piece: u32,
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
}

impl Lengths {
    pub fn from_torrent<ByteBuf: AsRef<[u8]>>(
        torrent: &TorrentMetaV1Info<ByteBuf>,
    ) -> anyhow::Result<Lengths> {
        let total_length = torrent.iter_file_lengths()?.sum();
        Lengths::new(total_length, torrent.piece_length)
    }

    pub fn new(total_length: u64, piece_length: u32) -> anyhow::Result<Self> {
        if total_length == 0 {
            anyhow::bail!("torrent with 0 length is useless")
        }
        let total_pieces = total_length.div_ceil(piece_length as u64) as u32;
        Ok(Self {
            piece_length,
            total_length,
            chunks_per_piece: (piece_length as u64).div_ceil(CHUNK_SIZE as u64) as u32,
            last_piece_id: total_pieces - 1,
            last_piece_length: last_element_size(total_length, piece_length as u64) as u32,
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
    pub const fn total_length(&self) -> u64 {
        self.total_length
    }
    pub const fn validate_piece_index(&self, index: u32) -> Option<ValidPieceIndex> {
        if index > self.last_piece_id {
            return None;
        }
        Some(ValidPieceIndex(index))
    }
    pub fn try_validate_piece_index(&self, index: u32) -> anyhow::Result<ValidPieceIndex> {
        self.validate_piece_index(index)
            .with_context(|| format!("invalid piece index {index}"))
    }
    pub const fn default_piece_length(&self) -> u32 {
        self.piece_length
    }
    pub const fn default_chunks_per_piece(&self) -> u32 {
        self.chunks_per_piece
    }
    pub const fn total_chunks(&self) -> u32 {
        // TODO: test
        self.last_piece_id * self.default_chunks_per_piece()
            + self.chunks_per_piece(self.last_piece_id())
    }
    pub const fn last_piece_id(&self) -> ValidPieceIndex {
        ValidPieceIndex(self.last_piece_id)
    }
    pub const fn total_pieces(&self) -> u32 {
        self.last_piece_id + 1
    }
    pub const fn piece_length(&self, index: ValidPieceIndex) -> u32 {
        if index.0 == self.last_piece_id {
            return self.last_piece_length;
        }
        self.piece_length
    }
    pub const fn chunk_absolute_offset(&self, chunk_info: &ChunkInfo) -> u64 {
        self.piece_offset(chunk_info.piece_index) + chunk_info.offset as u64
    }
    pub const fn piece_offset(&self, index: ValidPieceIndex) -> u64 {
        index.0 as u64 * self.piece_length as u64
    }

    pub fn iter_piece_infos(&self) -> impl Iterator<Item = PieceInfo> {
        let last_id = self.last_piece_id;
        let last_len = self.last_piece_length;
        let pl = self.piece_length;
        (0..self.total_pieces()).map(move |idx| PieceInfo {
            piece_index: ValidPieceIndex(idx),
            len: if idx == last_id { last_len } else { pl },
        })
    }

    // A helper to iterate over pieces in a file.
    pub(crate) fn iter_pieces_within_offset(
        &self,
        offset_bytes: u64,
        len: u64,
    ) -> std::ops::Range<u32> {
        // Validation and correction
        let offset_bytes = offset_bytes.min(self.total_length);
        let end_bytes = (offset_bytes + len).min(self.total_length);

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
    pub const fn chunk_range(&self, index: ValidPieceIndex) -> std::ops::Range<usize> {
        let start = index.0 * self.chunks_per_piece;
        let end = start + self.chunks_per_piece(index);
        start as usize..end as usize
    }
    pub const fn chunks_per_piece(&self, index: ValidPieceIndex) -> u32 {
        if index.0 == self.last_piece_id {
            return self.last_piece_length.div_ceil(CHUNK_SIZE);
        }
        self.chunks_per_piece
    }
    pub const fn chunk_offset_in_piece(
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
        let last_chunk_id = piece_length.div_ceil(CHUNK_SIZE) - 1;
        if chunk_index < last_chunk_id {
            return Some(CHUNK_SIZE);
        }
        if chunk_index == last_chunk_id {
            return Some(last_element_size(piece_length, CHUNK_SIZE));
        }
        return None;
    }

    // How many bytes out of the given piece are present in the given file (by offset and len).
    pub fn size_of_piece_in_file(&self, piece_id: u32, file_offset: u64, file_len: u64) -> u64 {
        let piece_offset = piece_id as u64 * self.default_piece_length() as u64;
        let piece_end = piece_offset + self.default_piece_length() as u64;

        let file_end = file_offset + file_len;

        let offset = file_offset.max(piece_offset);
        let end = file_end.min(piece_end);

        end.saturating_sub(offset)
    }
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
                    assert!(l.total_length() % l.default_piece_length() as u64 > 0);
                }
                if b {
                    assert_eq!(l.default_piece_length() % CHUNK_SIZE, 0)
                } else {
                    assert!(l.default_piece_length() % CHUNK_SIZE > 0)
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

        let l = Lengths::new(16384_1_1, 16384_1).unwrap();
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
                    $expected
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
}
