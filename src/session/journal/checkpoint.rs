//! Checkpoint payload codec + retention bookkeeping (PLAN §5.3,
//! PLAN2 S1.6 step 2).
//!
//! Lifted verbatim from `session/journal/mod.rs`. The `Checkpoint`
//! struct, `encode_checkpoint`/`decode_checkpoint` codec, anchor
//! helpers, and the gated `retain_before` + `retain_before_unchecked`
//! retention entry points are byte-identical to the previous inline
//! definitions. Visibility was inverted to `pub(crate)` only where
//! peer modules reach the items — see inline comments below.

use std::{
    fs,
    io::{self, Read, Seek},
    path::Path,
};

use super::{
    HEADER_LEN, JOURNAL_DIR_NAME, JournalCursor, RECORD_MAGIC, RecordKind, RetiredIncarnation,
};
use super::{
    incarnation_parts, list_incarnations, list_segments, retired_incarnations, segment_path,
    sync_dir,
};
// (no test-only imports beyond what's reachable via `super::*`)
// ---------------------------------------------------------------------------
// Checkpoints (RecordKind::CheckpointRef)
//
// A checkpoint payload is a versioned, self-describing restore anchor
// (PLAN §5.3): it carries the terminal geometry, cursor, modes, and a
// side-effect-free *restore program* (styled scrollback + clear + styled
// screen + cursor report) that repaints equivalent state into a fresh
// engine. Retention is gated on checkpoints: everything older than the
// newest checkpoint's incarnation may be deleted, because replay can
// start at the checkpoint instead.
// ---------------------------------------------------------------------------

pub const CHECKPOINT_VERSION: u16 = 2;
pub(crate) const CHECKPOINT_MAGIC: &[u8; 4] = b"OJCK";

/// One checkpoint payload, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub rows: u16,
    pub cols: u16,
    /// 1-based (row, col) cursor position.
    pub cursor: (u16, u16),
    pub alt_screen: bool,
    pub app_cursor_keys: bool,
    pub bracketed_paste: bool,
    /// Filtered display-stream offset covered by this checkpoint (v2):
    /// replay may start at the checkpoint's journal position and treat the
    /// derived stream as beginning at this offset (PLAN §5.3). `0` means
    /// unknown — records written by checkpoint format v1 carry no offset.
    pub filtered_offset: u64,
    /// Side-effect-free restore program (repaint escape stream).
    pub program: bytes::Bytes,
}

pub fn encode_checkpoint(checkpoint: &Checkpoint) -> bytes::Bytes {
    let mut out = Vec::with_capacity(27 + checkpoint.program.len());
    out.extend_from_slice(CHECKPOINT_MAGIC);
    out.extend_from_slice(&CHECKPOINT_VERSION.to_le_bytes());
    out.extend_from_slice(&checkpoint.rows.to_le_bytes());
    out.extend_from_slice(&checkpoint.cols.to_le_bytes());
    out.extend_from_slice(&checkpoint.cursor.0.to_le_bytes());
    out.extend_from_slice(&checkpoint.cursor.1.to_le_bytes());
    let flags = u8::from(checkpoint.alt_screen)
        | u8::from(checkpoint.app_cursor_keys) << 1
        | u8::from(checkpoint.bracketed_paste) << 2;
    out.push(flags);
    out.extend_from_slice(&checkpoint.filtered_offset.to_le_bytes());
    out.extend_from_slice(&(checkpoint.program.len() as u32).to_le_bytes());
    out.extend_from_slice(&checkpoint.program);
    bytes::Bytes::from(out)
}

#[cfg(test)]
pub fn decode_checkpoint(payload: &[u8]) -> io::Result<Checkpoint> {
    fn take<'a>(payload: &mut &'a [u8], n: usize) -> io::Result<&'a [u8]> {
        if payload.len() < n {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated checkpoint payload",
            ));
        }
        let (head, tail) = payload.split_at(n);
        *payload = tail;
        Ok(head)
    }
    let mut rest = payload;
    if take(&mut rest, 4)? != CHECKPOINT_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad checkpoint magic",
        ));
    }
    let version = u16::from_le_bytes(take(&mut rest, 2)?.try_into().unwrap());
    if !(1..=CHECKPOINT_VERSION).contains(&version) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported checkpoint version {version}"),
        ));
    }
    let rows = u16::from_le_bytes(take(&mut rest, 2)?.try_into().unwrap());
    let cols = u16::from_le_bytes(take(&mut rest, 2)?.try_into().unwrap());
    let cursor_row = u16::from_le_bytes(take(&mut rest, 2)?.try_into().unwrap());
    let cursor_col = u16::from_le_bytes(take(&mut rest, 2)?.try_into().unwrap());
    let flags = take(&mut rest, 1)?[0];
    let filtered_offset = if version >= 2 {
        u64::from_le_bytes(take(&mut rest, 8)?.try_into().unwrap())
    } else {
        0 // v1 records predate filtered-offset anchoring
    };
    let program_len = u32::from_le_bytes(take(&mut rest, 4)?.try_into().unwrap()) as usize;
    let program = take(&mut rest, program_len)?;
    if !rest.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing bytes after checkpoint program",
        ));
    }
    Ok(Checkpoint {
        rows,
        cols,
        cursor: (cursor_row, cursor_col),
        alt_screen: flags & 1 != 0,
        app_cursor_keys: flags & 2 != 0,
        bracketed_paste: flags & 4 != 0,
        filtered_offset,
        program: bytes::Bytes::copy_from_slice(program),
    })
}

/// One replay anchor: a checkpoint's filtered-stream offset and its
/// journal position (PLAN §5.3). Sorted by filtered offset ascending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointAnchor {
    pub filtered_offset: u64,
    pub cursor: JournalCursor,
    /// Segment part holding the checkpoint record.
    pub part: u64,
    /// Byte offset just past the checkpoint record within `part`: replay
    /// resumes scanning here (the record itself is never replayed).
    pub record_end_offset: u64,
}

/// Fixed header length of a v2 checkpoint payload before the restore
/// program: magic, version, rows, cols, cursor (row, col), flags,
/// filtered offset, program length.
const CHECKPOINT_FIXED_HEADER_LEN: usize = 4 + 2 + 2 + 2 + 2 + 2 + 1 + 8 + 4;

/// Scan one incarnation for checkpoint anchors, header-only: every record
/// header is validated in order (continuity is part of the anchor's
/// meaning), but payloads are skipped by seek except for the fixed
/// checkpoint header, which carries the filtered offset.
pub fn checkpoint_anchors(
    session_dir: &Path,
    incarnation: u64,
) -> io::Result<Vec<CheckpointAnchor>> {
    let journal_dir = session_dir.join(JOURNAL_DIR_NAME);
    let parts = incarnation_parts(&journal_dir, incarnation)?;
    let mut anchors = Vec::new();
    let mut expected_seq = 1u64;
    'parts: for (part, path) in &parts {
        let mut file = fs::File::open(path)?;
        let mut byte_pos = 0u64;
        let mut header = [0u8; HEADER_LEN];
        loop {
            match file.read_exact(&mut header) {
                Ok(()) => {}
                // End of this part (sealed parts end cleanly; a torn tail
                // only exists on the live last part): advance to the next
                // part. Anchors only come from validated records.
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(err) => return Err(err),
            }
            if &header[0..4] != RECORD_MAGIC {
                break 'parts;
            }
            let seq = u64::from_le_bytes(header[12..20].try_into().unwrap());
            if seq != expected_seq {
                break 'parts; // continuity hole: trust nothing further
            }
            let kind = u16::from_le_bytes(header[6..8].try_into().unwrap());
            let payload_len = u32::from_le_bytes(header[28..32].try_into().unwrap()) as u64;
            let record_end = byte_pos + HEADER_LEN as u64 + payload_len;
            if kind == RecordKind::CheckpointRef as u16 {
                // Only the fixed header is needed; skip the restore
                // program (which can be MiBs) entirely.
                if payload_len >= CHECKPOINT_FIXED_HEADER_LEN as u64 {
                    let mut fixed = [0u8; CHECKPOINT_FIXED_HEADER_LEN];
                    if file.read_exact(&mut fixed).is_err() {
                        break 'parts;
                    }
                    let version = u16::from_le_bytes(fixed[4..6].try_into().unwrap());
                    if version >= 2 {
                        let filtered_offset = u64::from_le_bytes(fixed[15..23].try_into().unwrap());
                        anchors.push(CheckpointAnchor {
                            filtered_offset,
                            cursor: JournalCursor { incarnation, seq },
                            part: *part,
                            record_end_offset: record_end,
                        });
                    }
                    let skip = payload_len - CHECKPOINT_FIXED_HEADER_LEN as u64;
                    if file.seek_relative(skip as i64).is_err() {
                        break 'parts;
                    }
                } else if file.seek_relative(payload_len as i64).is_err() {
                    break 'parts;
                }
            } else if file.seek_relative(payload_len as i64).is_err() {
                break 'parts;
            }
            byte_pos = record_end;
            expected_seq += 1;
        }
    }
    Ok(anchors)
}

/// The incarnation holding the newest checkpoint record, if any. Scans
/// incarnations newest-first, header-only, stopping at torn tails — this
/// runs at retention time, never on the hot path.
pub fn latest_checkpoint_incarnation(session_dir: &Path) -> io::Result<Option<u64>> {
    let journal_dir = session_dir.join(JOURNAL_DIR_NAME);
    for incarnation in list_incarnations(&journal_dir)?.into_iter().rev() {
        if incarnation_has_checkpoint(&journal_dir, incarnation)? {
            return Ok(Some(incarnation));
        }
    }
    Ok(None)
}

pub(crate) fn incarnation_has_checkpoint(journal_dir: &Path, incarnation: u64) -> io::Result<bool> {
    let mut parts: Vec<u64> = list_segments(journal_dir)?
        .into_iter()
        .filter(|&(inc, _)| inc == incarnation)
        .map(|(_, part)| part)
        .collect();
    parts.sort_unstable();
    for part in parts {
        let mut file = fs::File::open(segment_path(journal_dir, incarnation, part))?;
        let mut header = [0u8; HEADER_LEN];
        loop {
            match file.read_exact(&mut header) {
                Ok(()) => {}
                // Torn tail / EOF: nothing usable further in this part.
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(err) => return Err(err),
            }
            if &header[0..4] != RECORD_MAGIC {
                break;
            }
            let kind = u16::from_le_bytes(header[6..8].try_into().unwrap());
            let payload_len = u32::from_le_bytes(header[28..32].try_into().unwrap()) as u64;
            if kind == RecordKind::CheckpointRef as u16 {
                return Ok(true);
            }
            if file.seek_relative(payload_len as i64).is_err() {
                break;
            }
        }
    }
    Ok(false)
}

/// Checkpoint-gated retention (ADR-0002): delete sealed incarnations
/// below `min_incarnation`, but never past the newest checkpoint — the
/// checkpoint's restore program reconstructs the first exposed boundary,
/// and it lives in its own incarnation, so strictly older incarnations
/// are the only ones ever removed. With no checkpoint on record nothing
/// is deleted. Returns the deleted incarnation numbers.
pub fn retain_before(session_dir: &Path, min_incarnation: u64) -> io::Result<Vec<u64>> {
    let Some(gate) = latest_checkpoint_incarnation(session_dir)? else {
        return Ok(Vec::new());
    };
    let min_incarnation = min_incarnation.min(gate);
    let journal_dir = session_dir.join(JOURNAL_DIR_NAME);
    let incarnations = list_incarnations(&journal_dir)?;
    let latest = incarnations.last().copied();
    let already_retired = retired_incarnations(&journal_dir)?;
    let mut deleted = Vec::new();
    for incarnation in incarnations {
        if incarnation >= min_incarnation
            || Some(incarnation) == latest
            || already_retired.contains(&incarnation)
        {
            continue;
        }
        // Tombstone FIRST: the manifest records the intent before any
        // bytes disappear, so a crash mid-retention is distinguishable
        // from corruption (verification treats a missing tombstoned part
        // as intended, and `open` finishes an interrupted deletion).
        RetiredIncarnation {
            retired: incarnation,
        }
        .append_to(&journal_dir)?;
        // Delete newest-part-first: a concurrent reader can then only
        // ever observe a prefix of the incarnation (cursors stay
        // truthful), never a hole in the middle.
        for (_, part) in list_segments(&journal_dir)?
            .into_iter()
            .filter(|&(inc, _)| inc == incarnation)
            .rev()
        {
            fs::remove_file(segment_path(&journal_dir, incarnation, part))?;
        }
        deleted.push(incarnation);
    }
    if !deleted.is_empty() {
        sync_dir(&journal_dir)?;
    }
    Ok(deleted)
}

/// Unchecked retention primitive: delete **all parts** of the sealed
/// incarnations below `min_incarnation`. The latest incarnation is never
/// deleted (it may be active). Returns the deleted incarnation numbers.
/// Cursors into removed incarnations fail loudly on read (see
/// [`read_history`]); they never alias newer bytes.
///
/// **Test/dev plumbing only.** Production retention goes through
/// [`retain_before`], which clamps the deletion horizon to the newest
/// checkpoint (ADR-0002: a sealed incarnation is deleted only once a
/// retained checkpoint can reconstruct the first exposed boundary).
#[cfg(test)]
pub fn retain_before_unchecked(session_dir: &Path, min_incarnation: u64) -> io::Result<Vec<u64>> {
    let journal_dir = session_dir.join(JOURNAL_DIR_NAME);
    let incarnations = list_incarnations(&journal_dir)?;
    let latest = incarnations.last().copied();
    let mut deleted = Vec::new();
    for incarnation in incarnations {
        if incarnation >= min_incarnation || Some(incarnation) == latest {
            continue;
        }
        // Delete newest-part-first: a concurrent reader can then only
        // ever observe a prefix of the incarnation (cursors stay
        // truthful), never a hole in the middle.
        for (_, part) in list_segments(&journal_dir)?
            .into_iter()
            .filter(|&(inc, _)| inc == incarnation)
            .rev()
        {
            fs::remove_file(segment_path(&journal_dir, incarnation, part))?;
        }
        deleted.push(incarnation);
    }
    sync_dir(&journal_dir)?;
    Ok(deleted)
}
