use crate::{constants::CHUNK_SIZE, torrent_metainfo::TorrentMetaV1Info};

const fn is_power_of_two(x: u64) -> bool {
    (x != 0) && ((x & (x - 1)) == 0)
}

pub const fn ceil_div_u64(a: u64, b: u64) -> u64 {
    (a + b - 1) / b
}

pub const fn last_element_size_u64(total: u64, chunk_size: u64) -> u64 {
    let rem = total % chunk_size;
    if rem == 0 {
        return chunk_size;
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
    pub chunk_index: u32,
    pub absolute_index: u32,
    pub size: u32,
    pub offset: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Lengths {
    chunk_length: u32,
    total_length: u64,
    piece_length: u32,
    last_piece_id: u32,
    last_piece_length: u32,
    max_chunks_per_piece: u32,
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
    pub fn get(&self) -> u32 {
        self.0
    }
}

impl Lengths {
    pub fn from_torrent<ByteBuf: AsRef<[u8]>>(
        torrent: &TorrentMetaV1Info<ByteBuf>,
    ) -> anyhow::Result<Lengths> {
        let total_length = torrent.iter_file_lengths()?.sum();
        Lengths::new(total_length, torrent.piece_length, None)
    }

    pub fn new(
        total_length: u64,
        piece_length: u32,
        chunk_length: Option<u32>,
    ) -> anyhow::Result<Self> {
        let chunk_length = chunk_length.unwrap_or(CHUNK_SIZE);
        // I guess this is not needed? Don't recall why I put this check here.
        //
        // if !(is_power_of_two(piece_length as u64)) {
        //     anyhow::bail!("piece length {} is not a power of 2", piece_length);
        // }
        if !(is_power_of_two(chunk_length as u64)) {
            anyhow::bail!("chunk length {} is not a power of 2", chunk_length);
        }
        if chunk_length > piece_length {
            anyhow::bail!(
                "chunk length {} should be >= piece length {}",
                chunk_length,
                piece_length
            );
        }
        if total_length == 0 {
            anyhow::bail!("torrent with 0 length")
        }
        let total_pieces = ceil_div_u64(total_length, piece_length as u64) as u32;
        Ok(Self {
            chunk_length,
            piece_length,
            total_length,
            max_chunks_per_piece: ceil_div_u64(piece_length as u64, chunk_length as u64) as u32,
            last_piece_id: total_pieces - 1,
            last_piece_length: last_element_size_u64(total_length, piece_length as u64) as u32,
        })
    }
    pub const fn piece_bitfield_bytes(&self) -> usize {
        ceil_div_u64(self.total_pieces() as u64, 8) as usize
    }
    pub const fn chunk_bitfield_bytes(&self) -> usize {
        ceil_div_u64(self.total_chunks() as u64, 8) as usize
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
    pub const fn default_piece_length(&self) -> u32 {
        self.piece_length
    }
    pub const fn default_chunk_length(&self) -> u32 {
        self.chunk_length
    }
    pub const fn default_max_chunks_per_piece(&self) -> u32 {
        self.max_chunks_per_piece
    }
    pub const fn total_chunks(&self) -> u32 {
        ceil_div_u64(self.total_length, self.chunk_length as u64) as u32
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

    pub fn iter_chunk_infos(&self, index: ValidPieceIndex) -> impl Iterator<Item = ChunkInfo> {
        let mut remaining = self.piece_length(index);
        let chunk_size = self.chunk_length;
        let absolute_offset = index.0 * self.max_chunks_per_piece;
        (0u32..).scan(0, move |offset, idx| {
            if remaining == 0 {
                return None;
            }
            let s = std::cmp::min(remaining, chunk_size);
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
        let index = begin / self.chunk_length;
        let expected_chunk_size = self.chunk_size(piece_index, index)?;
        let offset = self.chunk_offset_in_piece(piece_index, index)?;
        if offset != begin {
            return None;
        }
        if expected_chunk_size != chunk_size {
            return None;
        }
        let absolute_index = self.max_chunks_per_piece * piece_index.get() + index;
        Some(ChunkInfo {
            piece_index,
            chunk_index: index,
            size: chunk_size,
            offset,
            absolute_index,
        })
    }

    pub fn chunk_info_from_received_piece(
        &self,
        index: u32,
        begin: u32,
        block_len: u32,
    ) -> Option<ChunkInfo> {
        self.chunk_info_from_received_data(self.validate_piece_index(index)?, begin, block_len)
    }
    pub const fn chunk_range(&self, index: ValidPieceIndex) -> std::ops::Range<usize> {
        let start = index.0 * self.max_chunks_per_piece;
        let end = start + self.chunks_per_piece(index);
        start as usize..end as usize
    }
    pub const fn chunks_per_piece(&self, index: ValidPieceIndex) -> u32 {
        if index.0 == self.last_piece_id {
            return (self.last_piece_length + self.chunk_length - 1) / self.chunk_length;
        }
        self.max_chunks_per_piece
    }
    pub const fn chunk_offset_in_piece(
        &self,
        piece_index: ValidPieceIndex,
        chunk_index: u32,
    ) -> Option<u32> {
        if chunk_index >= self.chunks_per_piece(piece_index) {
            return None;
        }
        Some(chunk_index * self.chunk_length)
    }
    pub fn chunk_size(&self, piece_index: ValidPieceIndex, chunk_index: u32) -> Option<u32> {
        let chunks_per_piece = self.chunks_per_piece(piece_index);
        let pl = self.piece_length(piece_index);
        if chunk_index >= chunks_per_piece {
            return None;
        }
        let offset = chunk_index * self.chunk_length;
        Some(std::cmp::min(self.chunk_length, pl - offset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_lengths() -> Lengths {
        Lengths::new(1174243328, 262144, None).unwrap()
    }

    #[test]
    fn test_total_pieces() {
        let l = make_lengths();
        assert_eq!(l.total_pieces(), 4480);
    }

    #[test]
    fn test_total_pieces_2() {
        let l = Lengths::new(4148166656, 2097152, None).unwrap();
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
}
