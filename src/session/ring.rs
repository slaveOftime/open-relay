use bytes::Bytes;
use std::collections::VecDeque;

/// A byte-limited ring buffer for raw PTY output chunks.
///
/// Each chunk is stored with its absolute byte offset within the overall
/// output stream.  When inserting a new chunk would exceed `capacity_bytes`,
/// the oldest chunks are evicted first to make room.
pub struct RingBuffer {
    /// `(start_byte_offset_of_chunk, raw_bytes)` in insertion order.
    chunks: VecDeque<(u64, Bytes)>,
    /// Total bytes ever written — equals the end offset of the last chunk.
    total_bytes_written: u64,
    /// Maximum combined byte length held simultaneously.
    capacity_bytes: usize,
    /// Running sum of byte lengths across all chunks currently held.
    current_size: usize,
}

impl RingBuffer {
    pub fn new(capacity_bytes: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            total_bytes_written: 0,
            capacity_bytes: capacity_bytes.max(1),
            current_size: 0,
        }
    }

    /// Push a new raw chunk.  Oldest chunks are evicted until the data fits
    /// within `capacity_bytes`.
    pub fn push(&mut self, data: Bytes) {
        if data.is_empty() {
            return;
        }
        let start_offset = self.total_bytes_written;
        let data_len = data.len();

        // Evict oldest chunks until the incoming data fits.
        while self.current_size + data_len > self.capacity_bytes && !self.chunks.is_empty() {
            if let Some((_, evicted)) = self.chunks.pop_front() {
                self.current_size -= evicted.len();
            }
        }

        self.chunks.push_back((start_offset, data));
        self.current_size += data_len;
        self.total_bytes_written += data_len as u64;
    }

    /// Return all chunks whose data overlaps `[from_offset, ∞)` together with
    /// the current end offset.  A chunk that only partially overlaps the range
    /// is trimmed so the returned slice starts exactly at `from_offset`.
    ///
    /// If `from_offset` is before `start_offset()` all chunks are returned
    /// (the evicted prefix is simply unavailable).
    pub fn read_from(&self, from_offset: u64) -> (Vec<(u64, Bytes)>, u64) {
        let chunks = self
            .chunks
            .iter()
            .filter(|(start, data)| start + data.len() as u64 > from_offset)
            .map(|(start, data)| {
                let skip = from_offset.saturating_sub(*start) as usize;
                (start + skip as u64, data.slice(skip..))
            })
            .collect();
        (chunks, self.total_bytes_written)
    }

    /// Byte offset of the first byte still retained in the ring.
    /// Returns `total_bytes_written` when the ring is empty.
    #[allow(dead_code)]
    pub fn start_offset(&self) -> u64 {
        self.chunks
            .front()
            .map(|(off, _)| *off)
            .unwrap_or(self.total_bytes_written)
    }

    /// Byte offset immediately past the last retained byte (== total written).
    pub fn end_offset(&self) -> u64 {
        self.total_bytes_written
    }

    /// Iterate over every stored chunk in insertion order.
    /// Useful for feeding all buffered output into a vt100 parser.
    pub fn all_chunks(&self) -> impl Iterator<Item = &Bytes> {
        self.chunks.iter().map(|(_, data)| data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_read_all() {
        let mut ring = RingBuffer::new(1024);
        ring.push(Bytes::from_static(b"hello "));
        ring.push(Bytes::from_static(b"world"));
        let (chunks, end) = ring.read_from(0);
        let combined: Vec<u8> = chunks.iter().flat_map(|(_, d)| d.iter().copied()).collect();
        assert_eq!(combined, b"hello world");
        assert_eq!(end, 11);
    }

    #[test]
    fn evicts_oldest_chunks_to_fit_capacity() {
        let mut ring = RingBuffer::new(10);
        ring.push(Bytes::from_static(b"12345")); // offset 0..5
        ring.push(Bytes::from_static(b"67890")); // offset 5..10
        // Adding 4 bytes needs eviction (10 + 4 > 10).
        ring.push(Bytes::from_static(b"ABCD"));
        assert_eq!(ring.start_offset(), 5);
        assert_eq!(ring.end_offset(), 14);
    }

    #[test]
    fn read_from_mid_chunk_trims_correctly() {
        let mut ring = RingBuffer::new(1024);
        ring.push(Bytes::from_static(b"abcde")); // offset 0..5
        ring.push(Bytes::from_static(b"fghij")); // offset 5..10
        let (chunks, end) = ring.read_from(3);
        let combined: Vec<u8> = chunks.iter().flat_map(|(_, d)| d.iter().copied()).collect();
        assert_eq!(combined, b"defghij");
        assert_eq!(end, 10);
        assert_eq!(chunks[0].0, 3); // trimmed chunk starts at offset 3
    }

    #[test]
    fn read_from_past_end_returns_empty() {
        let mut ring = RingBuffer::new(1024);
        ring.push(Bytes::from_static(b"hello"));
        let (chunks, end) = ring.read_from(100);
        assert!(chunks.is_empty());
        assert_eq!(end, 5);
    }

    #[test]
    fn start_offset_zero_when_empty() {
        let ring = RingBuffer::new(1024);
        assert_eq!(ring.start_offset(), 0);
        assert_eq!(ring.end_offset(), 0);
    }
}
