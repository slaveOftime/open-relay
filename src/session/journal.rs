//! Segmented session journal: typed, checksummed records with stable
//! sequence numbers (PLAN.md §6.1, invariants I1/I3/I8).
//!
//! This module is M1 groundwork: it implements the record format, segment
//! writer/reader and torn-tail recovery. Nothing wires it into the live
//! session pipeline yet — `output.log` remains the canonical store until the
//! sequencer lands and the attach path reads exact committed ranges.
//!
//! Format (all integers little-endian):
//!
//! ```text
//! record := magic(4) version(u16) kind(u16) flags(u32)
//!           seq(u64) elapsed_ms(u64) payload_len(u32) crc32(u32) payload
//! ```
//!
//! `crc32` (IEEE) covers the header bytes before the CRC field plus the
//! payload. `seq` is strictly monotonic and never reused **within an
//! incarnation**; the full cursor is `{session_id, incarnation, seq}` —
//! unlike the `output.log` offsets that `truncate_output_log` reuses today.
//! `elapsed_ms` is assigned by the sequencer (monotonic time since the
//! incarnation started); the session's wall-clock launch time lives in the
//! manifest, not per record.

use std::{
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
};

const RECORD_MAGIC: &[u8; 4] = b"OJRN";
const RECORD_VERSION: u16 = 1;
/// magic + version + kind + flags + seq + elapsed_ms + payload_len + crc32.
const HEADER_LEN: usize = 4 + 2 + 2 + 4 + 8 + 8 + 4 + 4;
/// Refuse absurd length fields before allocating (PLAN.md §7.4: limits are
/// checked before allocation).
const MAX_PAYLOAD_LEN: u32 = 64 * 1024 * 1024;

/// Typed journal record kinds. Unknown kinds are unrecoverable corruption for
/// this format version: a torn or aliased tail must stop the scan, never be
/// silently skipped (I3 — missing history is explicit, not empty success).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum RecordKind {
    /// Original PTY output bytes.
    Output = 1,
    /// A successful PTY resize barrier: `rows(u16) cols(u16)` payload.
    Resize = 2,
    /// Lifecycle transition (spawned/completed/capture state), UTF-8 payload.
    Lifecycle = 3,
    /// Points at the checkpoint that reconstructs state at `seq`.
    CheckpointRef = 4,
    /// Terminal profile/policy facts needed to interpret the stream.
    Policy = 5,
}

impl RecordKind {
    fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Output),
            2 => Some(Self::Resize),
            3 => Some(Self::Lifecycle),
            4 => Some(Self::CheckpointRef),
            5 => Some(Self::Policy),
            _ => None,
        }
    }
}

/// One decoded journal record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub kind: RecordKind,
    pub seq: u64,
    /// Milliseconds since the writer's start (monotonic, per incarnation).
    pub elapsed_ms: u64,
    pub payload: Vec<u8>,
}

// ---------------------------------------------------------------------------
// CRC-32 (IEEE 802.3, reflected) — hand-rolled to avoid a new dependency;
// verified against the standard check vector in the tests below.
// ---------------------------------------------------------------------------

const fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = crc32_table();

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc = CRC32_TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Append handle for one journal segment. Sequences are allocated here, in
/// append order, starting at the `first_seq` chosen by the caller. Event
/// timing is **not** owned by the writer: the sequencer passes `elapsed_ms`
/// in, so reopening an active segment can never move event time backwards.
pub struct SegmentWriter {
    file: fs::File,
    next_seq: u64,
    /// Bytes written so far; the authoritative segment length.
    written: u64,
}

impl SegmentWriter {
    pub fn create(path: &Path, first_seq: u64) -> io::Result<Self> {
        Ok(Self {
            file: fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(path)?,
            next_seq: first_seq,
            written: 0,
        })
    }

    pub fn open_append(path: &Path, next_seq: u64) -> io::Result<Self> {
        let file = fs::OpenOptions::new().append(true).open(path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            file,
            next_seq,
            written,
        })
    }

    /// Append one record, returning its sequence number. `elapsed_ms` is the
    /// sequencer's monotonic time since incarnation start.
    pub fn append(&mut self, kind: RecordKind, elapsed_ms: u64, payload: &[u8]) -> io::Result<u64> {
        if payload.len() > MAX_PAYLOAD_LEN as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "journal payload of {} bytes exceeds the {} byte limit",
                    payload.len(),
                    MAX_PAYLOAD_LEN
                ),
            ));
        }
        let seq = self.next_seq;

        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(RECORD_MAGIC);
        header[4..6].copy_from_slice(&RECORD_VERSION.to_le_bytes());
        header[6..8].copy_from_slice(&(kind as u16).to_le_bytes());
        // flags [8..12] stay zero until a feature needs them.
        header[12..20].copy_from_slice(&seq.to_le_bytes());
        header[20..28].copy_from_slice(&elapsed_ms.to_le_bytes());
        header[28..32].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let crc = crc32_two(&header[..32], payload);
        header[32..36].copy_from_slice(&crc.to_le_bytes());

        self.file.write_all(&header)?;
        self.file.write_all(payload)?;
        self.written += (HEADER_LEN + payload.len()) as u64;
        self.next_seq += 1;
        Ok(seq)
    }

    /// Push appended records to the storage device. Callers decide the
    /// group-sync cadence (PLAN.md §4.2): live publication does not wait for
    /// this, but `durable_seq` may not advance past the last synced record.
    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }

    /// Bytes written so far — the segment's authoritative length.
    pub fn len(&self) -> u64 {
        self.written
    }

    pub fn is_empty(&self) -> bool {
        self.written == 0
    }

    /// Sequence number the next appended record will get.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }
}

fn crc32_two(first: &[u8], second: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in first.iter().chain(second) {
        crc = CRC32_TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

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
pub fn scan_segment(path: &Path) -> io::Result<ScanOutcome> {
    let mut file = fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut records = Vec::new();
    let mut offset = 0u64;
    let mut expected_seq: Option<u64> = None;

    loop {
        let mut header = [0u8; HEADER_LEN];
        match read_exact_or_partial(&mut file, &mut header)? {
            ReadPiece::Complete => {}
            ReadPiece::Partial => return Ok(outcome(records, offset, ScanStop::PartialTail)),
            ReadPiece::Empty => {
                debug_assert_eq!(offset, file_len, "EOF only at the real end");
                return Ok(outcome(records, offset, ScanStop::CleanEof));
            }
        }

        if &header[..4] != RECORD_MAGIC {
            return Ok(outcome(records, offset, ScanStop::InvalidHeader));
        }
        if u16::from_le_bytes(header[4..6].try_into().unwrap()) != RECORD_VERSION {
            return Ok(outcome(records, offset, ScanStop::UnsupportedVersion));
        }
        let kind = match RecordKind::from_u16(u16::from_le_bytes(header[6..8].try_into().unwrap()))
        {
            Some(kind) => kind,
            None => return Ok(outcome(records, offset, ScanStop::UnknownKind)),
        };
        let payload_len = u32::from_le_bytes(header[28..32].try_into().unwrap());
        if payload_len > MAX_PAYLOAD_LEN {
            return Ok(outcome(records, offset, ScanStop::OversizeLength));
        }

        let mut payload = vec![0u8; payload_len as usize];
        match read_exact_or_partial(&mut file, &mut payload)? {
            ReadPiece::Complete => {}
            ReadPiece::Partial | ReadPiece::Empty => {
                return Ok(outcome(records, offset, ScanStop::PartialTail));
            }
        }

        let stored_crc = u32::from_le_bytes(header[32..36].try_into().unwrap());
        if crc32_two(&header[..32], &payload) != stored_crc {
            return Ok(outcome(records, offset, ScanStop::CrcMismatch));
        }

        let seq = u64::from_le_bytes(header[12..20].try_into().unwrap());
        if let Some(expected) = expected_seq
            && seq != expected
        {
            // A sequence gap means an earlier record was lost or the tail was
            // aliased: stop here so recovery never presents a silent hole.
            return Ok(outcome(records, offset, ScanStop::SequenceDiscontinuity));
        }
        expected_seq = Some(seq + 1);

        records.push(Record {
            kind,
            seq,
            elapsed_ms: u64::from_le_bytes(header[20..28].try_into().unwrap()),
            payload,
        });
        offset += (HEADER_LEN + payload_len as usize) as u64;
        file.seek(SeekFrom::Start(offset))?;
    }
}

fn outcome(records: Vec<Record>, valid_len: u64, stop: ScanStop) -> ScanOutcome {
    ScanOutcome {
        records,
        valid_len,
        stop,
    }
}

enum ReadPiece {
    Complete,
    Partial,
    Empty,
}

/// Distinguish a clean EOF at a record boundary (`Empty`) from a torn read
/// (`Partial`); both end the scan at the last valid record.
fn read_exact_or_partial(file: &mut fs::File, buf: &mut [u8]) -> io::Result<ReadPiece> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("oly_journal_test_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn crc32_matches_the_standard_check_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn written_records_scan_back_identically() {
        let path = test_path("roundtrip.seg");
        let _ = fs::remove_file(&path);

        let mut writer = SegmentWriter::create(&path, 7).unwrap();
        assert_eq!(writer.append(RecordKind::Output, 0, b"hello").unwrap(), 7);
        writer
            .append(RecordKind::Resize, 1, &24u16.to_le_bytes())
            .unwrap();
        writer.append(RecordKind::Lifecycle, 1, b"").unwrap();
        writer.sync().unwrap();
        drop(writer);

        let scanned = scan_segment(&path).unwrap();
        assert_eq!(scanned.stop, ScanStop::CleanEof);
        assert_eq!(scanned.valid_len, fs::metadata(&path).unwrap().len());
        assert_eq!(scanned.records.len(), 3);
        assert_eq!(scanned.records[0].seq, 7);
        assert_eq!(scanned.records[0].kind, RecordKind::Output);
        assert_eq!(scanned.records[0].payload, b"hello");
        assert_eq!(scanned.records[1].kind, RecordKind::Resize);
        assert_eq!(scanned.records[2].seq, 9);
        assert!(scanned.records[2].payload.is_empty());

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn torn_tail_recovers_to_the_last_valid_record() {
        let path = test_path("torn.seg");
        let _ = fs::remove_file(&path);

        let mut writer = SegmentWriter::create(&path, 0).unwrap();
        writer.append(RecordKind::Output, 0, b"first").unwrap();
        writer.append(RecordKind::Output, 0, b"second").unwrap();
        let valid_len = writer.len();
        writer.append(RecordKind::Output, 0, b"third-torn").unwrap();
        drop(writer);

        // Simulate a crash mid-write: keep only half of the third record.
        let file_len = fs::metadata(&path).unwrap().len();
        let torn_len = valid_len + (file_len - valid_len) / 2;
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(torn_len).unwrap();
        drop(file);

        let scanned = scan_segment(&path).unwrap();
        assert_eq!(scanned.stop, ScanStop::PartialTail);
        assert!(scanned.stop.may_rewind(), "an active torn tail may rewind");
        assert_eq!(scanned.valid_len, valid_len);
        assert_eq!(scanned.records.len(), 2);
        assert_eq!(scanned.records[1].payload, b"second");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn corrupted_payload_stops_the_scan() {
        let path = test_path("corrupt.seg");
        let _ = fs::remove_file(&path);

        let mut writer = SegmentWriter::create(&path, 0).unwrap();
        writer.append(RecordKind::Output, 0, b"good").unwrap();
        let first_len = writer.len();
        writer.append(RecordKind::Output, 0, b"bad").unwrap();
        writer.append(RecordKind::Output, 0, b"after").unwrap();
        drop(writer);

        // Flip one payload byte of the middle record.
        let mut bytes = fs::read(&path).unwrap();
        bytes[first_len as usize + HEADER_LEN] ^= 0xFF;
        fs::write(&path, &bytes).unwrap();

        let scanned = scan_segment(&path).unwrap();
        assert_eq!(scanned.stop, ScanStop::CrcMismatch);
        assert!(
            !scanned.stop.may_rewind(),
            "CRC corruption must be quarantined, not silently truncated"
        );
        assert_eq!(scanned.valid_len, first_len);
        assert_eq!(scanned.records.len(), 1);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn sequence_gap_stops_the_scan_without_a_hole() {
        let path = test_path("seqgap.seg");
        let _ = fs::remove_file(&path);

        let mut writer = SegmentWriter::create(&path, 0).unwrap();
        writer.append(RecordKind::Output, 0, b"a").unwrap();
        let first_len = writer.len();
        writer.append(RecordKind::Output, 0, b"b").unwrap();
        drop(writer);

        // Rewrite the second record's seq as if a later segment tail had
        // been aliased into place: the scan must stop, not skip ahead.
        let mut bytes = fs::read(&path).unwrap();
        let seq_at = first_len as usize + 12;
        bytes[seq_at..seq_at + 8].copy_from_slice(&42u64.to_le_bytes());
        // Re-seal the CRC so only the continuity check can catch this.
        let header_end = first_len as usize + 32;
        let payload = bytes[first_len as usize + HEADER_LEN..].to_vec();
        let crc = crc32_two(&bytes[first_len as usize..header_end], &payload);
        bytes[first_len as usize + 32..first_len as usize + 36].copy_from_slice(&crc.to_le_bytes());
        fs::write(&path, &bytes).unwrap();

        let scanned = scan_segment(&path).unwrap();
        assert_eq!(scanned.stop, ScanStop::SequenceDiscontinuity);
        assert!(!scanned.stop.may_rewind());
        assert_eq!(scanned.records.len(), 1);
        assert_eq!(scanned.valid_len, first_len);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn oversized_payload_is_rejected_before_writing() {
        let path = test_path("oversized.seg");
        let _ = fs::remove_file(&path);

        let mut writer = SegmentWriter::create(&path, 0).unwrap();
        let huge = vec![0u8; MAX_PAYLOAD_LEN as usize + 1];
        assert!(writer.append(RecordKind::Output, 0, &huge).is_err());
        assert_eq!(writer.len(), 0, "rejected record must not write bytes");
        assert_eq!(writer.next_seq(), 0, "rejected record must not spend a seq");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn garbage_length_field_does_not_allocate() {
        // A torn/aliased tail can present an enormous payload_len; the scan
        // must reject it against the limit before sizing a buffer.
        let path = test_path("garbage_len.seg");
        let _ = fs::remove_file(&path);

        let mut writer = SegmentWriter::create(&path, 0).unwrap();
        writer.append(RecordKind::Output, 0, b"only").unwrap();
        let valid_len = writer.len();
        drop(writer);

        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(RECORD_MAGIC);
        header[4..6].copy_from_slice(&RECORD_VERSION.to_le_bytes());
        header[6..8].copy_from_slice(&(RecordKind::Output as u16).to_le_bytes());
        header[12..20].copy_from_slice(&1u64.to_le_bytes());
        header[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&header).unwrap();
        drop(file);

        let scanned = scan_segment(&path).unwrap();
        assert_eq!(scanned.stop, ScanStop::OversizeLength);
        assert!(!scanned.stop.may_rewind());
        assert_eq!(scanned.valid_len, valid_len);
        assert_eq!(scanned.records.len(), 1);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn writer_records_the_sequencer_assigned_elapsed_ms() {
        // The writer never invents event timing (reopening a segment must
        // not move event time backwards); the sequencer owns the clock.
        let path = test_path("elapsed.seg");
        let _ = fs::remove_file(&path);

        let mut writer = SegmentWriter::create(&path, 0).unwrap();
        for elapsed in 0..16u64 {
            writer.append(RecordKind::Output, elapsed, b"x").unwrap();
        }
        drop(writer);

        let scanned = scan_segment(&path).unwrap();
        for (index, record) in scanned.records.iter().enumerate() {
            assert_eq!(record.elapsed_ms, index as u64);
        }

        let _ = fs::remove_file(&path);
    }
}
