//! Reader / torn-tail recovery, segment scan internals, and ScanResult types
//! (PLAN2 S1.5 step 3 + S1.6 step 1 + P2.2).
//!
//! Two contiguous `mod.rs` blocks were carved verbatim:
//!   1. The 105-line on-the-wire scan API (`ScanStop`, `ScanOutcome`,
//!      `scan_segment`, `SegmentStats`, `scan_segment_stats`,
//!      `scan_segment_stats_from`).
//!   2. The 224-line scan internals (`ScanMode`, `ScanResult`,
//!      `ScanStart`, `scan_impl`, `outcome`, `ReadPiece`,
//!      `read_exact_or_partial`).
//!
//! No body code was rewritten in S1.5/S1.6. P2.2 then rewrote the
//! payload-validation path inside `scan_impl` to stream-check the CRC
//! over a 1 MiB scratch buffer before allocating the payload Vec, so a
//! corrupt-but-valid header never forces a 64 MiB allocation.
//!
//! Visibility widened to `pub(crate)` on items reached from sibling
//! submodules (`stream`, `segment`, `appender`, `open`) and from
//! `mod.rs`'s own `mod tests` block.

use std::{
    fs, io,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

#[allow(unused_imports)]
use super::IndexEntry;
use super::{
    CollectWindow, Crc32, HEADER_LEN, MAX_PAYLOAD_LEN, RECORD_MAGIC, RECORD_VERSION, Record,
    RecordKind,
}; // used as field type at line 159 (cfg(test) for index_entries.push)

/// Chunk size used by the streaming CRC pre-check inside `scan_impl`
/// (PLAN2 P2.2). A corrupt-but-valid header claiming a 64 MiB payload
/// is now diagnosed after reading `MAX_PAYLOAD_LEN / SCAN_HASH_WINDOW`
/// `1 MiB` chunks; the surviving payload allocation is gated on the
/// CRC passing. The valid path reads each payload twice — once to
/// hash, once to materialise — but verification/recovery is a cold
/// path and the re-read is bounded by `MAX_PAYLOAD_LEN`.
pub(crate) const SCAN_HASH_WINDOW: usize = 1024 * 1024;
// ---------------------------------------------------------------------------
// Reader / torn-tail recovery
// ---------------------------------------------------------------------------

/// Why a segment scan stopped. Recovery distinguishes a torn **active tail**
/// (rewind to `valid_len`) from **corruption** (quarantine and report — never
/// silently truncate). Segment state (active versus sealed) is owned by the
/// caller; a non-tail stop in a sealed segment is always corruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanStop {
    /// The file ends exactly at a record boundary.
    CleanEof,
    /// The last record is incomplete (crash mid-write). Rewind to
    /// `valid_len` on an active segment.
    PartialTail,
    /// Magic bytes do not match the journal format.
    InvalidHeader,
    /// The record version is not supported by this reader.
    UnsupportedVersion,
    /// The record kind is unknown to this format version.
    UnknownKind,
    /// The declared payload length exceeds the allocation bound.
    OversizeLength,
    /// The stored CRC does not match header plus payload.
    CrcMismatch,
    /// A record's sequence number is not the previous one plus one — an
    /// earlier record was lost or the tail was aliased.
    SequenceDiscontinuity,
}

impl ScanStop {
    /// Only a partial tail may be rewound by truncation (on an active
    /// segment); everything else must be quarantined and reported.
    pub fn may_rewind(self) -> bool {
        matches!(self, ScanStop::PartialTail)
    }
}

/// Result of scanning a segment: every fully valid record in order, the byte
/// length of the valid prefix, and why the scan stopped.
#[derive(Debug)]
pub struct ScanOutcome {
    pub records: Vec<Record>,
    /// Byte offset one past the last valid record. On recovery the caller
    /// truncates an active segment to this length (torn tail) or quarantines
    /// the file (any other stop reason).
    pub valid_len: u64,
    pub stop: ScanStop,
}

impl ScanOutcome {
    #[cfg(test)]
    pub fn is_clean(&self) -> bool {
        self.stop == ScanStop::CleanEof
    }
}

/// Scan a segment from the start, stopping at the first invalid byte.
///
/// Recovery contract (PLAN.md §6.2): a crash can tear the *tail* of the
/// active segment; recovery rewinds to `valid_len`
/// (`ScanStop::PartialTail`). Anything else is corruption, not a tear — the
/// caller quarantines and reports it instead of silently continuing.
#[cfg(test)]
pub fn scan_segment(path: &Path) -> io::Result<ScanOutcome> {
    // Lenient on the first sequence: this is the raw segment inspector
    // (recovery tooling, probes, tests). Stream-level reads enforce the
    // expected first sequence themselves via `scan_segment_stats*` and
    // the read APIs below.
    Ok(scan_impl(path, ScanMode::All, None, None)?.outcome)
}

/// Payload-free segment statistics: record count, last sequence, valid
/// prefix length and stop reason. Recovery and cursor validation use this
/// so their memory stays O(1) regardless of segment size.
#[derive(Debug)]
pub struct SegmentStats {
    pub records: u64,
    pub last_seq: Option<u64>,
    pub valid_len: u64,
    pub stop: ScanStop,
}

/// Stats scan requiring the segment to start at sequence 1 (a complete
/// incarnation prefix).
#[cfg(test)]
pub fn scan_segment_stats(path: &Path) -> io::Result<SegmentStats> {
    scan_segment_stats_from(path, Some(1))
}

/// Stats scan with an explicit expected first sequence (`None` accepts any
/// first record; used for continuation segments and partial inspection).
pub fn scan_segment_stats_from(
    path: &Path,
    expected_first_seq: Option<u64>,
) -> io::Result<SegmentStats> {
    let result = scan_impl(path, ScanMode::Stats, expected_first_seq, None)?;
    Ok(SegmentStats {
        records: result.record_count,
        last_seq: result.last_seq,
        valid_len: result.outcome.valid_len,
        stop: result.outcome.stop,
    })
}

// =========================================================================
// Scan internals (carved from mod.rs at S1.6 step 1).
// Body byte-identical; visibility widened as documented inline.
// =========================================================================

/// What a scan collects. All modes validate the consumed prefix fully
/// (headers, CRCs, continuity) — they differ only in what they retain.
pub(crate) enum ScanMode {
    /// Keep every record (small segments and tests only — recovery uses
    /// [`ScanMode::Stats`] so open-time memory does not scale with the
    /// recording).
    #[cfg(test)]
    All,
    /// Keep only records inside the seq window, under a byte budget.
    Window(CollectWindow),
    /// Keep only per-record index entries (bounded tail/seek support).
    #[cfg(test)]
    Index,
    /// Keep nothing but counters: O(1) memory regardless of segment size.
    Stats,
}

/// Everything one pass over a segment learned. `outcome.records` is only
/// populated in `All`/`Window` modes and `index` only in `Index` mode;
/// `record_count`/`last_seq` are tracked in every mode.
pub(crate) struct ScanResult {
    pub(crate) outcome: ScanOutcome,
    pub(crate) truncated: bool,
    #[cfg(test)]
    pub(crate) index: Vec<IndexEntry>,
    pub(crate) record_count: u64,
    pub(crate) last_seq: Option<u64>,
}

/// Byte offset and expected sequence to resume a scan from (sparse-index
/// seek). The skipped prefix was validated when the segment part was
/// sealed — or is being written by the appender for the live part — and
/// is re-validated whenever a read needs it; CRC and continuity checks
/// apply from `offset` onward.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScanStart {
    pub(crate) offset: u64,
    pub(crate) seq: u64,
}

/// Sparse index granularity: one entry per at most this many bytes of
/// segment data. Keeps index memory O(part_size / stride) — 64 entries
/// for a default 64 MiB part — while bounding a tail read's seek region
/// to `max_bytes + stride`.
#[allow(dead_code)] // reached from `stream::read_tail` via `super::SPARSE_INDEX_STRIDE_BYTES`
pub(crate) const SPARSE_INDEX_STRIDE_BYTES: u64 = 1024 * 1024;

pub(crate) fn scan_impl(
    path: &Path,
    mode: ScanMode,
    expected_first_seq: Option<u64>,
    start: Option<ScanStart>,
) -> io::Result<ScanResult> {
    let mut file = fs::File::open(path)?;
    let mut records = Vec::new();
    #[cfg(test)]
    let mut index_entries = Vec::new();
    let mut buffered_bytes = 0usize;
    let mut truncated = false;
    let mut offset = 0u64;
    let mut expected_seq: Option<u64> = expected_first_seq;
    if let Some(start) = start {
        file.seek(SeekFrom::Start(start.offset))?;
        offset = start.offset;
        expected_seq = Some(start.seq);
    }
    let mut record_count = 0u64;
    let mut last_seq: Option<u64> = None;

    macro_rules! stop {
        ($reason:expr) => {
            return Ok(ScanResult {
                outcome: outcome(records, offset, $reason),
                truncated,
                #[cfg(test)]
                index: index_entries,
                record_count,
                last_seq,
            })
        };
    }

    loop {
        let mut header = [0u8; HEADER_LEN];
        match read_exact_or_partial(&mut file, &mut header)? {
            ReadPiece::Complete => {}
            ReadPiece::Partial => stop!(ScanStop::PartialTail),
            ReadPiece::Empty => {
                // On a live segment the file may have grown since the scan
                // started; `offset` remains the end of what was validated.
                stop!(ScanStop::CleanEof);
            }
        }

        if &header[..4] != RECORD_MAGIC {
            stop!(ScanStop::InvalidHeader);
        }
        if u16::from_le_bytes(header[4..6].try_into().unwrap()) != RECORD_VERSION {
            stop!(ScanStop::UnsupportedVersion);
        }
        let kind = match RecordKind::from_u16(u16::from_le_bytes(header[6..8].try_into().unwrap()))
        {
            Some(kind) => kind,
            None => stop!(ScanStop::UnknownKind),
        };
        let payload_len = u32::from_le_bytes(header[28..32].try_into().unwrap());
        if payload_len > MAX_PAYLOAD_LEN {
            stop!(ScanStop::OversizeLength);
        }

        let seq = u64::from_le_bytes(header[12..20].try_into().unwrap());
        if let ScanMode::Window(window) = &mode
            && seq > window.to_seq
        {
            // Past the requested window: the whole window was present and
            // contiguous. Nothing beyond it needs validation here.
            stop!(ScanStop::CleanEof);
        }

        // PLAN2 P2.2: stream-check the payload CRC in 1 MiB windows
        // before allocating the full `payload_len` Vec. The on-disk cap
        // `MAX_PAYLOAD_LEN` is still enforced (`payload_len <= it` was
        // checked above), but the pre-check means a corrupt-but-valid
        // header claiming 64 MiB only consumes `SCAN_HASH_WINDOW`
        // bytes of stack space; the big allocation is gated on the
        // CRC passing.
        let payload_start_offset = offset + HEADER_LEN as u64;
        let mut hasher = Crc32::new();
        // The stored CRC covers `header[..32] + payload` (see
        // `encode_record_header`). Mix the header into the streaming
        // hash so a CRC mismatch here really means "the bytes on disk
        // disagree with what the header declared", not "we forgot
        // 32 bytes".
        hasher.update(&header[..32]);
        let mut remaining = payload_len as usize;
        // Fixed-size scratch on the stack keeps the working set bounded
        // regardless of `payload_len`.
        let mut scratch = [0u8; SCAN_HASH_WINDOW];
        let mut torn_tail = false;
        while remaining > 0 {
            let want = scratch.len().min(remaining);
            match read_exact_or_partial(&mut file, &mut scratch[..want])? {
                ReadPiece::Complete => {}
                // A short read on a payload that hasn't finished is a
                // torn tail by definition: the record claims more bytes
                // than the segment holds.
                ReadPiece::Partial | ReadPiece::Empty => torn_tail = true,
            }
            if torn_tail {
                break;
            }
            hasher.update(&scratch[..want]);
            remaining -= want;
        }

        if torn_tail {
            stop!(ScanStop::PartialTail);
        }

        let stored_crc = u32::from_le_bytes(header[32..36].try_into().unwrap());
        if hasher.finish() != stored_crc {
            // CRC mismatch — never allocated the big payload buffer.
            // The file is already at `payload_start_offset + payload_len`
            // (where `offset += ...` would push it), so the loop footer
            // can continue cleanly.
            stop!(ScanStop::CrcMismatch);
        }

        // CRC matched. Re-read the payload into a properly-sized Vec so
        // downstream consumers (`ScanMode::All`, `Window`) can keep it.
        // The seek is bounded by payload_len and the file is now stream-
        // friendly (linear forward reads).
        file.seek(SeekFrom::Start(payload_start_offset))?;
        let mut payload = vec![0u8; payload_len as usize];
        match read_exact_or_partial(&mut file, &mut payload)? {
            ReadPiece::Complete => {}
            ReadPiece::Partial | ReadPiece::Empty => stop!(ScanStop::PartialTail),
        }

        if let Some(expected) = expected_seq
            && seq != expected
        {
            // A sequence gap (or a first record that is not the expected
            // first sequence of this stream) means an earlier record was
            // lost or the tail was aliased: stop here so recovery never
            // presents a silent hole.
            stop!(ScanStop::SequenceDiscontinuity);
        }
        expected_seq = Some(seq + 1);
        record_count += 1;
        last_seq = Some(seq);

        match &mode {
            #[cfg(test)]
            ScanMode::All => {
                records.push(Record {
                    kind,
                    seq,
                    elapsed_ms: u64::from_le_bytes(header[20..28].try_into().unwrap()),
                    payload,
                });
            }
            ScanMode::Window(window) => {
                if seq >= window.from_seq {
                    let over_budget = !records.is_empty()
                        && buffered_bytes + payload.len() > window.max_buffered_bytes;
                    if over_budget {
                        truncated = true;
                        stop!(ScanStop::CleanEof);
                    }
                    buffered_bytes += payload.len();
                    records.push(Record {
                        kind,
                        seq,
                        elapsed_ms: u64::from_le_bytes(header[20..28].try_into().unwrap()),
                        payload,
                    });
                }
            }
            #[cfg(test)]
            ScanMode::Index => {
                let due = index_entries
                    .last()
                    .is_none_or(|entry| offset >= entry.offset + SPARSE_INDEX_STRIDE_BYTES);
                if due {
                    index_entries.push(IndexEntry {
                        seq,
                        offset,
                        record_len: HEADER_LEN as u64 + payload_len as u64,
                    });
                }
            }
            ScanMode::Stats => {}
        }
        offset += (HEADER_LEN + payload_len as usize) as u64;
        file.seek(SeekFrom::Start(offset))?;
    }
}

pub(crate) fn outcome(records: Vec<Record>, valid_len: u64, stop: ScanStop) -> ScanOutcome {
    ScanOutcome {
        records,
        valid_len,
        stop,
    }
}

pub(crate) enum ReadPiece {
    Complete,
    Partial,
    Empty,
}

/// Distinguish a clean EOF at a record boundary (`Empty`) from a torn read
/// (`Partial`); both end the scan at the last valid record.
pub(crate) fn read_exact_or_partial(file: &mut fs::File, buf: &mut [u8]) -> io::Result<ReadPiece> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => {
                return Ok(if filled == 0 {
                    ReadPiece::Empty
                } else {
                    ReadPiece::Partial
                });
            }
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(ReadPiece::Complete)
}
