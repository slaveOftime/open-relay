//! On-disk segment writer and segment-file helpers (PLAN2 S1.5 step 5).
//!
//! Lifted from `session/journal/mod.rs`. The active-tail writer
//! (`SegmentWriter`) and segment-path / list-segment helpers
//! (`segment_path`, `parse_segment_name`, `list_segments`,
//! `list_incarnations`, `part_first_seq`) and segment-naming constants
//! (`JOURNAL_DIR_NAME`, `SEGMENT_PREFIX`, `MANIFEST_FILE_NAME`,
//! `SEGMENT_SUFFIX`, `DEFAULT_SEGMENT_MAX_BYTES`) is byte-identical to
//! the previous inline definitions.
//!
//! Visibility widened `fn` → `pub(crate) fn` for `segment_path`,
//! `parse_segment_name`, `part_first_seq`, and `SEGMENT_PREFIX` /
//! `SEGMENT_SUFFIX` so the new sibling modules (manifest, scan, stream,
//! appender) can call them without crossing the journal crate boundary
//! twice. No external caller can observe the difference: they were
//! `crate-internal` before, they still are.

use std::{
    fs, io,
    io::Write,
    path::{Path, PathBuf},
};

use super::{HEADER_LEN, RECORD_MAGIC, ReadPiece, read_exact_or_partial};
#[cfg(test)]
use super::{MAX_PAYLOAD_LEN, RecordKind, encode_record_header};

// Writer
// ---------------------------------------------------------------------------

/// Append handle for one journal segment. The writer is a pure serializer:
/// the **sequencer** assigns `seq` and `elapsed_ms` before the event is
/// published, and the writer writes what it is given. It never allocates
/// sequences and never invents timestamps, so reopening an active segment
/// can never move event time backwards and publication never waits on disk.
pub struct SegmentWriter {
    file: fs::File,
    /// Bytes written so far; the authoritative segment length.
    written: u64,
}

impl SegmentWriter {
    pub fn create(path: &Path) -> io::Result<Self> {
        Ok(Self {
            file: fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(path)?,
            written: 0,
        })
    }

    // Only exercised by the linux-gated disk-full test in journal/mod.rs.
    #[cfg(all(test, target_os = "linux"))]
    pub fn open_append(path: &Path) -> io::Result<Self> {
        let file = fs::OpenOptions::new().append(true).open(path)?;
        let written = file.metadata()?.len();
        Ok(Self { file, written })
    }

    #[cfg(test)]
    /// Append one already-sequenced record.
    pub fn append_record(
        &mut self,
        kind: RecordKind,
        seq: u64,
        elapsed_ms: u64,
        payload: &[u8],
    ) -> io::Result<()> {
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

        let header = encode_record_header(kind, seq, elapsed_ms, payload)?;
        self.write_encoded(&header, payload)
    }

    /// Append one pre-encoded record (header built by
    /// [`encode_record_header`]). The rolling writer uses this so it can
    /// checksum the exact bytes for the sealed-part manifest without
    /// re-reading the file.
    pub fn write_encoded(&mut self, header: &[u8; HEADER_LEN], payload: &[u8]) -> io::Result<()> {
        self.file.write_all(header)?;
        self.file.write_all(payload)?;
        self.written += (HEADER_LEN + payload.len()) as u64;
        Ok(())
    }

    /// Push appended records to the storage device. Callers decide the
    /// group-sync cadence (PLAN.md §4.2): live publication does not wait for
    /// this, but `durable_seq` may not advance past the last synced record.
    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }

    #[cfg(test)]
    /// Bytes written so far — the segment's authoritative length.
    pub fn len(&self) -> u64 {
        self.written
    }
}

// ---------------------------------------------------------------------------
// Per-session sequencer (M1)
// ---------------------------------------------------------------------------

/// Directory inside `sessions/<id>/` holding the journal segments.
pub const JOURNAL_DIR_NAME: &str = "journal";
const SEGMENT_PREFIX: &str = "seg-";
/// Append-only manifest of sealed segment parts (M3-6): one JSON line per
/// sealed part, written after the part itself is durable. Compaction (M4)
/// and integrity tooling verify parts against these entries; the active
/// tail part never has one.
pub const MANIFEST_FILE_NAME: &str = "manifest.log";
const SEGMENT_SUFFIX: &str = ".ojrn";

/// Maximum size of one segment part before the appender rolls over to the
/// next part of the same incarnation. Bounding part size bounds recovery
/// work, per-read validation work and sparse-index memory (ADR-0002).
pub const DEFAULT_SEGMENT_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Segments are split into bounded parts within one incarnation:
/// `seg-{incarnation:08}-{part:04}.ojrn`. Sequences continue across
/// parts, so cursors stay `{incarnation, seq}` and never name parts.
pub(crate) fn segment_path(journal_dir: &Path, incarnation: u64, part: u64) -> PathBuf {
    journal_dir.join(format!(
        "{SEGMENT_PREFIX}{incarnation:08}-{part:04}{SEGMENT_SUFFIX}"
    ))
}

fn parse_segment_name(name: &str) -> Option<(u64, u64)> {
    let rest = name
        .strip_prefix(SEGMENT_PREFIX)?
        .strip_suffix(SEGMENT_SUFFIX)?;
    let (incarnation, part) = rest.split_once('-')?;
    Some((incarnation.parse().ok()?, part.parse().ok()?))
}

/// All `(incarnation, part)` pairs in a journal directory, ascending.
pub fn list_segments(journal_dir: &Path) -> io::Result<Vec<(u64, u64)>> {
    let mut segments = Vec::new();
    if !journal_dir.exists() {
        return Ok(segments);
    }
    for entry in fs::read_dir(journal_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pair) = parse_segment_name(name) else {
            continue;
        };
        segments.push(pair);
    }
    segments.sort_unstable();
    Ok(segments)
}

/// All incarnation numbers present in a journal directory, ascending.
pub fn list_incarnations(journal_dir: &Path) -> io::Result<Vec<u64>> {
    let segments = list_segments(journal_dir)?;
    let mut incarnations: Vec<u64> = segments
        .iter()
        .map(|&(incarnation, _)| incarnation)
        .collect();
    incarnations.dedup();
    Ok(incarnations)
}

/// Sequence number of a segment part's first record, read from its header
/// only (O(1)); `None` for an empty part (a crash between rollover and
/// the first append can leave one as the newest part).
pub(crate) fn part_first_seq(path: &Path) -> io::Result<Option<u64>> {
    let mut file = fs::File::open(path)?;
    let mut header = [0u8; HEADER_LEN];
    match read_exact_or_partial(&mut file, &mut header)? {
        ReadPiece::Empty => Ok(None),
        ReadPiece::Partial => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: torn first record header", path.display()),
        )),
        ReadPiece::Complete => {
            if header[0..4] != *RECORD_MAGIC {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: bad journal magic", path.display()),
                ));
            }
            Ok(Some(u64::from_le_bytes(
                header[12..20].try_into().expect("seq field"),
            )))
        }
    }
}
