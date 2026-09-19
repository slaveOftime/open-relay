//! Turning a persisted output stream into addressable, paginated records.
//!
//! Raw PTY logs have no line framing, so records are cut on terminal-aware
//! boundaries. Journal-backed sessions paginate the derived filtered stream
//! via the streaming replay reader (M6-2); the legacy `output.log` sidecar
//! index is retired.

#[cfg(test)]
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
#[cfg(test)]
use std::io::{Seek, SeekFrom};
use std::path::Path;

#[cfg(test)]
use crate::error::Result;
use crate::protocol::LogResize;

use super::{ESCAPE_BYTE, OUTPUT_COLOR_RESET_SUFFIX, ViewportReplayPlan};

/// Fallback record size for raw PTY log pagination when no natural terminal
/// boundary appears for a long stretch of bytes.
pub(super) const LOG_RECORD_FALLBACK_BYTES: usize = 2048;

#[derive(Clone, Debug, Default)]
struct LogRecordScannerState {
    current_record: Vec<u8>,
    pending_escape: Vec<u8>,
}

pub(super) struct TailBytes {
    pub(super) bytes: Vec<u8>,
    pub(super) start_offset: u64,
    pub(super) end_offset: u64,
}

/// Read a page of lines from a session's persisted output.
///
/// Returns `Ok((records, total_record_count))`, or `Ok(None)` when the
/// session has no persisted output at all. For raw PTY streams, records are
/// split on terminal-aware boundaries first and fall back to fixed-size
/// chunks when the stream contains no `\n`.
///
/// Journal-backed sessions paginate the derived filtered stream via the
/// streaming replay reader (bounded memory; M6-2). A session with only a
/// pre-0.5 `output.log` is an explicit error — the legacy fallback is
/// retired (see MIGRATION.md).
pub fn read_persisted_log_page(
    session_dir: &Path,
    offset: usize,
    limit: usize,
) -> std::result::Result<Option<(Vec<String>, usize)>, String> {
    if session_dir
        .join(crate::session::journal::JOURNAL_DIR_NAME)
        .is_dir()
    {
        let reader = crate::session::replay::ReplayReader::new(session_dir)
            .map_err(|err| format!("failed to open the session journal: {err}"))?;
        let mut page = PaginatedLogRecords::new(offset, limit);
        scan_persisted_log_records(reader, |record| page.push(record))
            .map_err(|err| format!("failed to scan the session journal: {err}"))?;
        return Ok(Some(page.finish()));
    }

    if session_dir.join("output.log").exists() {
        return Err(format!(
            "session log in {} uses the pre-0.5 format (output.log) and is no \
             longer readable; export it with a 0.x build first, see MIGRATION.md",
            session_dir.display()
        ));
    }

    Ok(None)
}

pub fn split_rendered_log_output(output: &[u8]) -> Vec<String> {
    let output = output
        .strip_suffix(OUTPUT_COLOR_RESET_SUFFIX)
        .unwrap_or(output);

    let mut chunks = Vec::new();
    let mut start = 0usize;

    for (index, &byte) in output.iter().enumerate() {
        if byte == b'\n' {
            chunks.push(String::from_utf8_lossy(&output[start..=index]).into_owned());
            start = index + 1;
        }
    }

    if start < output.len() {
        if let Some(last) = chunks.last_mut() {
            last.push_str(&String::from_utf8_lossy(&output[start..]));
        } else {
            chunks.push(String::from_utf8_lossy(&output[start..]).into_owned());
        }
    }

    chunks
}

#[cfg(test)]
pub(super) fn split_persisted_log_records(bytes: &[u8]) -> Vec<String> {
    let mut records = Vec::new();
    scan_persisted_log_records(std::io::Cursor::new(bytes), |record| {
        records.push(String::from_utf8_lossy(record).into_owned());
    })
    .expect("scan in-memory log bytes");
    records
}

struct PaginatedLogRecords {
    offset: usize,
    end: usize,
    total: usize,
    records: Vec<String>,
}

impl PaginatedLogRecords {
    fn new(offset: usize, limit: usize) -> Self {
        Self {
            offset,
            end: offset.saturating_add(limit),
            total: 0,
            records: Vec::with_capacity(limit),
        }
    }

    fn push(&mut self, record: &[u8]) {
        if record.is_empty() {
            return;
        }

        if self.total >= self.offset && self.total < self.end {
            self.records
                .push(String::from_utf8_lossy(record).into_owned());
        }
        self.total += 1;
    }

    fn finish(self) -> (Vec<String>, usize) {
        (self.records, self.total)
    }
}

fn scan_persisted_log_records<R, F>(reader: R, on_record: F) -> std::io::Result<()>
where
    R: Read,
    F: FnMut(&[u8]),
{
    let mut scanner = LogRecordScanner::new(on_record);
    process_persisted_log_reader(reader, &mut scanner)?;
    scanner.finish();
    Ok(())
}

fn process_persisted_log_reader<R, F>(
    reader: R,
    scanner: &mut LogRecordScanner<F>,
) -> std::io::Result<()>
where
    R: Read,
    F: FnMut(&[u8]),
{
    let mut reader = BufReader::new(reader);

    loop {
        let consumed = {
            let chunk = reader.fill_buf()?;
            if chunk.is_empty() {
                break;
            }

            scanner.process_bytes(chunk);
            chunk.len()
        };
        reader.consume(consumed);
    }

    Ok(())
}

struct LogRecordScanner<F>
where
    F: FnMut(&[u8]),
{
    state: LogRecordScannerState,
    on_record: F,
}

impl<F> LogRecordScanner<F>
where
    F: FnMut(&[u8]),
{
    fn new(on_record: F) -> Self {
        Self {
            state: LogRecordScannerState::default(),
            on_record,
        }
    }

    fn process_bytes(&mut self, bytes: &[u8]) {
        let mut remaining = bytes;

        while !remaining.is_empty() {
            if !self.state.pending_escape.is_empty() {
                let consumed = self.process_pending_escape_bytes(remaining);
                remaining = &remaining[consumed..];
                continue;
            }

            let Some(special_index) = find_special_record_byte(remaining) else {
                self.push_plain_bytes(remaining);
                break;
            };

            self.push_plain_bytes(&remaining[..special_index]);

            match remaining[special_index] {
                b'\n' | b'\r' => {
                    self.state.current_record.push(remaining[special_index]);
                    self.flush_current_record();
                }
                ESCAPE_BYTE => self.state.pending_escape.push(ESCAPE_BYTE),
                _ => unreachable!("special record byte lookup returned unsupported byte"),
            }

            remaining = &remaining[special_index + 1..];
        }
    }

    fn process_pending_escape_bytes(&mut self, bytes: &[u8]) -> usize {
        for (index, &byte) in bytes.iter().enumerate() {
            self.state.pending_escape.push(byte);

            match ansi_sequence_status(&self.state.pending_escape) {
                AnsiSequenceStatus::Incomplete => {}
                AnsiSequenceStatus::Complete => {
                    let is_boundary = is_record_boundary_sequence(&self.state.pending_escape);
                    self.flush_pending_escape(is_boundary);
                    return index + 1;
                }
                AnsiSequenceStatus::Invalid => {
                    self.flush_pending_escape(false);
                    return index + 1;
                }
            }
        }

        bytes.len()
    }

    fn push_plain_bytes(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let available =
                LOG_RECORD_FALLBACK_BYTES.saturating_sub(self.state.current_record.len());
            if available == 0 {
                self.flush_current_record();
                continue;
            }

            if bytes.len() <= available {
                self.state.current_record.extend_from_slice(bytes);
                return;
            }

            let split_at = utf8_boundary_at_or_before(bytes, available).max(1);
            self.state
                .current_record
                .extend_from_slice(&bytes[..split_at]);
            self.flush_current_record();
            bytes = &bytes[split_at..];
        }
    }

    fn finish(&mut self) {
        if !self.state.pending_escape.is_empty() {
            let pending_escape = std::mem::take(&mut self.state.pending_escape);
            self.state.current_record.extend_from_slice(&pending_escape);
        }

        self.flush_current_record();
    }

    fn flush_current_record(&mut self) {
        if self.state.current_record.is_empty() {
            return;
        }

        (self.on_record)(&self.state.current_record);
        self.state.current_record.clear();
    }

    fn flush_pending_escape(&mut self, is_boundary: bool) {
        if is_boundary && !self.state.current_record.is_empty() {
            self.flush_current_record();
        }

        let pending_escape = std::mem::take(&mut self.state.pending_escape);
        self.state.current_record.extend_from_slice(&pending_escape);
    }
}

fn find_special_record_byte(bytes: &[u8]) -> Option<usize> {
    bytes
        .iter()
        .position(|&byte| matches!(byte, b'\n' | b'\r' | ESCAPE_BYTE))
}

fn utf8_boundary_at_or_before(bytes: &[u8], end: usize) -> usize {
    let mut candidate = end.min(bytes.len());
    while candidate > 0 && std::str::from_utf8(&bytes[..candidate]).is_err() {
        candidate -= 1;
    }

    candidate
}

enum AnsiSequenceStatus {
    Incomplete,
    Complete,
    Invalid,
}

fn ansi_sequence_status(bytes: &[u8]) -> AnsiSequenceStatus {
    if bytes.first().copied() != Some(ESCAPE_BYTE) {
        return AnsiSequenceStatus::Invalid;
    }

    let Some(second) = bytes.get(1).copied() else {
        return AnsiSequenceStatus::Incomplete;
    };

    match second {
        b'[' => {
            if bytes[2..].iter().any(|byte| (0x40..=0x7e).contains(byte)) {
                AnsiSequenceStatus::Complete
            } else {
                AnsiSequenceStatus::Incomplete
            }
        }
        b']' | b'P' | b'X' | b'^' | b'_' => {
            if bytes.last().copied() == Some(0x07)
                || (bytes.len() >= 2 && bytes[bytes.len() - 2..] == [0x1b, b'\\'])
            {
                AnsiSequenceStatus::Complete
            } else {
                AnsiSequenceStatus::Incomplete
            }
        }
        _ => AnsiSequenceStatus::Complete,
    }
}

fn is_record_boundary_sequence(sequence: &[u8]) -> bool {
    if sequence.len() < 3 || sequence[0] != ESCAPE_BYTE || sequence[1] != b'[' {
        return false;
    }

    let final_byte = *sequence.last().unwrap_or(&0);
    let params = &sequence[2..sequence.len() - 1];

    matches!(final_byte, b'H' | b'f' | b'd' | b'G' | b'J' | b'K')
        || is_alt_screen_toggle(params, final_byte)
}

fn is_alt_screen_toggle(params: &[u8], final_byte: u8) -> bool {
    matches!(final_byte, b'h' | b'l') && matches!(params, b"?1049" | b"?1047")
}

/// Seek near the end of the log file and read enough bytes to cover `tail * 2`
/// lines (using a generous per-line estimate), returning the raw bytes.
///
/// If the seek position doesn't land at byte 0, the first partial line is
/// dropped to avoid feeding truncated ANSI escape sequences into a downstream
/// parser (which can corrupt subsequent color state).
#[cfg(test)]
pub(super) fn read_tail_bytes(log_path: &Path, tail: usize) -> Result<TailBytes> {
    let mut file = File::open(log_path)?;
    let file_size = file.seek(SeekFrom::End(0))?;

    if file_size == 0 {
        return Ok(TailBytes {
            bytes: Vec::new(),
            start_offset: 0,
            end_offset: 0,
        });
    }

    // Check if file ends with newline to adjust our line counting
    file.seek(SeekFrom::End(-1))?;
    let mut last_byte = [0u8; 1];
    file.read_exact(&mut last_byte)?;
    let ends_with_newline = last_byte[0] == b'\n';

    // We want at least `tail * 2` lines, but ensure a minimum of 100 lines for context.
    let lines_needed = (tail * 2).max(100) + if ends_with_newline { 1 } else { 0 };

    let chunk_size = 64 * 1024; // 64KB chunks
    let mut position = file_size;
    let mut lines_found = 0;
    let mut buf = vec![0u8; chunk_size];

    while position > 0 && lines_found < lines_needed {
        let to_read = std::cmp::min(position, chunk_size as u64);
        position -= to_read;

        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut buf[..to_read as usize])?;

        let chunk = &buf[..to_read as usize];
        for (i, &byte) in chunk.iter().enumerate().rev() {
            if byte == b'\n' {
                lines_found += 1;
                if lines_found >= lines_needed {
                    // Start reading *after* this newline
                    position += (i as u64) + 1;
                    break;
                }
            }
        }

        if lines_found >= lines_needed {
            break;
        }
    }

    file.seek(SeekFrom::Start(position))?;
    let mut bytes = Vec::with_capacity((file_size - position) as usize);
    file.read_to_end(&mut bytes)?;

    Ok(TailBytes {
        bytes,
        start_offset: position,
        end_offset: file_size,
    })
}

/// In-memory equivalent of [`read_tail_bytes`] for journal-derived
/// streams (M3-1c): same tail-window and partial-line-drop semantics.
pub(super) fn tail_window_bytes(bytes: &[u8], tail: usize) -> TailBytes {
    if bytes.is_empty() {
        return TailBytes {
            bytes: Vec::new(),
            start_offset: 0,
            end_offset: 0,
        };
    }
    let ends_with_newline = bytes.last() == Some(&b'\n');
    let lines_needed = (tail * 2).max(100) + usize::from(ends_with_newline);

    let mut position = bytes.len();
    let mut lines_found = 0usize;
    for (i, &byte) in bytes.iter().enumerate().rev() {
        if byte == b'\n' {
            lines_found += 1;
            if lines_found >= lines_needed {
                position = i + 1;
                break;
            }
        }
    }
    if lines_found < lines_needed {
        position = 0;
    }

    TailBytes {
        bytes: bytes[position..].to_vec(),
        start_offset: position as u64,
        end_offset: bytes.len() as u64,
    }
}

/// Pick the resize records relevant to replaying the stream window
/// `[start_offset, end_offset)`: the geometry in effect at `start_offset`
/// plus every resize inside the window, rebased to window-relative
/// offsets. Pure derivation over journal-sourced resize events (M6-2).
pub(super) fn viewport_resize_plan(
    events: &[LogResize],
    start_offset: u64,
    end_offset: u64,
) -> ViewportReplayPlan {
    let mut initial = None;
    let mut resizes = Vec::new();
    for event in events {
        if event.offset <= start_offset {
            initial = Some(*event);
        } else if event.offset <= end_offset {
            resizes.push(LogResize {
                offset: event.offset.saturating_sub(start_offset),
                rows: event.rows,
                cols: event.cols,
            });
        } else {
            break;
        }
    }

    ViewportReplayPlan { initial, resizes }
}
