//! Turning a persisted `output.log` into addressable, paginated records.
//!
//! Raw PTY logs have no line framing, so records are cut on terminal-aware
//! boundaries and their offsets cached in a sidecar `output.log.idx` so
//! paging does not rescan the whole file.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{error::Result, protocol::LogResize};

use super::{ESCAPE_BYTE, OUTPUT_COLOR_RESET_SUFFIX, ViewportReplayPlan};

/// Fallback record size for raw PTY log pagination when no natural terminal
/// boundary appears for a long stretch of bytes.
pub(super) const LOG_RECORD_FALLBACK_BYTES: usize = 2048;

const LOG_INDEX_OFFSETS_FILE: &str = "output.log.idx";
const LOG_INDEX_META_FILE: &str = "output.log.idx.meta";

pub(super) struct TailBytes {
    pub(super) bytes: Vec<u8>,
    pub(super) start_offset: u64,
    pub(super) end_offset: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct LogRecordScannerState {
    current_record: Vec<u8>,
    pending_escape: Vec<u8>,
}

impl LogRecordScannerState {
    fn trailing_len(&self) -> u64 {
        (self.current_record.len() + self.pending_escape.len()) as u64
    }

    fn has_trailing_record(&self) -> bool {
        !self.current_record.is_empty() || !self.pending_escape.is_empty()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedLogIndexMeta {
    indexed_len: u64,
    scanner_state: LogRecordScannerState,
}

#[derive(Clone, Debug, Default)]
pub(super) struct PersistedLogIndex {
    indexed_len: u64,
    complete_record_end_offsets: Vec<u64>,
    scanner_state: LogRecordScannerState,
}

impl PersistedLogIndex {
    fn from_meta(meta: PersistedLogIndexMeta, complete_record_end_offsets: Vec<u64>) -> Self {
        Self {
            indexed_len: meta.indexed_len,
            complete_record_end_offsets,
            scanner_state: meta.scanner_state,
        }
    }

    fn to_meta(&self) -> PersistedLogIndexMeta {
        PersistedLogIndexMeta {
            indexed_len: self.indexed_len,
            scanner_state: self.scanner_state.clone(),
        }
    }

    fn total_records(&self) -> usize {
        self.complete_record_end_offsets.len()
            + usize::from(self.scanner_state.has_trailing_record())
    }

    fn last_complete_end_offset(&self) -> u64 {
        self.complete_record_end_offsets
            .last()
            .copied()
            .unwrap_or(0)
    }

    fn is_consistent_with(&self, file_len: u64) -> bool {
        self.indexed_len <= file_len
            && self.last_complete_end_offset() <= self.indexed_len
            && self.scanner_state.trailing_len()
                <= self
                    .indexed_len
                    .saturating_sub(self.last_complete_end_offset())
    }
}

/// Read a page of lines from a persisted `output.log`.
///
/// Returns `(records, total_record_count)` or `None` if the file can't be
/// opened. For raw PTY streams, records are split on terminal-aware boundaries
/// first and fall back to fixed-size chunks when the stream contains no `\n`.
pub fn read_persisted_log_page(
    session_dir: &Path,
    offset: usize,
    limit: usize,
) -> Option<(Vec<String>, usize)> {
    let log_path = session_dir.join("output.log");
    if let Ok(index) = sync_persisted_log_index(&log_path)
        && let Ok(records) = read_persisted_log_page_from_index(&log_path, &index, offset, limit)
    {
        return Some((records, index.total_records()));
    }

    let file = File::open(log_path).ok()?;
    let mut page = PaginatedLogRecords::new(offset, limit);
    scan_persisted_log_records(file, |record| page.push(record)).ok()?;
    Some(page.finish())
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
        Self::from_state(LogRecordScannerState::default(), on_record)
    }

    fn from_state(state: LogRecordScannerState, on_record: F) -> Self {
        Self { state, on_record }
    }

    fn into_state(self) -> LogRecordScannerState {
        self.state
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

fn read_persisted_log_page_from_index(
    log_path: &Path,
    index: &PersistedLogIndex,
    offset: usize,
    limit: usize,
) -> std::io::Result<Vec<String>> {
    let total = index.total_records();
    if limit == 0 || offset >= total {
        return Ok(Vec::new());
    }

    let end = offset.saturating_add(limit).min(total);
    let complete_count = index.complete_record_end_offsets.len();
    let start_offset = if offset == 0 {
        0
    } else {
        index.complete_record_end_offsets[offset - 1]
    };
    let end_offset = if end <= complete_count {
        index.complete_record_end_offsets[end - 1]
    } else {
        index.indexed_len
    };

    let mut file = File::open(log_path)?;
    file.seek(SeekFrom::Start(start_offset))?;

    let mut bytes = vec![0u8; (end_offset - start_offset) as usize];
    file.read_exact(&mut bytes)?;

    let complete_end = end.min(complete_count);
    let requested_complete_offsets = &index.complete_record_end_offsets[offset..complete_end];
    let mut records = Vec::with_capacity(end - offset);
    let mut record_start = 0usize;

    for &record_end in requested_complete_offsets {
        let relative_end = (record_end - start_offset) as usize;
        records.push(String::from_utf8_lossy(&bytes[record_start..relative_end]).into_owned());
        record_start = relative_end;
    }

    if end > complete_count && record_start < bytes.len() {
        records.push(String::from_utf8_lossy(&bytes[record_start..]).into_owned());
    }

    Ok(records)
}

pub(super) fn sync_persisted_log_index(log_path: &Path) -> std::io::Result<PersistedLogIndex> {
    let file_len = fs::metadata(log_path)?.len();

    let mut index = match load_persisted_log_index(log_path) {
        Ok(index) if index.is_consistent_with(file_len) => index,
        _ => return rebuild_persisted_log_index(log_path, file_len),
    };

    if index.indexed_len < file_len {
        let new_offsets = extend_persisted_log_index(log_path, &mut index, file_len)?;
        append_log_index_offsets(log_path, &new_offsets)?;
        write_log_index_meta(log_path, &index.to_meta())?;
    }

    Ok(index)
}

fn rebuild_persisted_log_index(
    log_path: &Path,
    file_len: u64,
) -> std::io::Result<PersistedLogIndex> {
    let mut index = PersistedLogIndex::default();
    let complete_offsets = extend_persisted_log_index(log_path, &mut index, file_len)?;
    index.complete_record_end_offsets = complete_offsets;
    write_log_index_offsets(log_path, &index.complete_record_end_offsets)?;
    write_log_index_meta(log_path, &index.to_meta())?;
    Ok(index)
}

fn extend_persisted_log_index(
    log_path: &Path,
    index: &mut PersistedLogIndex,
    file_len: u64,
) -> std::io::Result<Vec<u64>> {
    let mut file = File::open(log_path)?;
    file.seek(SeekFrom::Start(index.indexed_len))?;

    let mut completed_end = index
        .indexed_len
        .saturating_sub(index.scanner_state.trailing_len());
    let mut new_offsets = Vec::new();
    let state = std::mem::take(&mut index.scanner_state);
    let mut scanner = LogRecordScanner::from_state(state, |record| {
        completed_end += record.len() as u64;
        new_offsets.push(completed_end);
    });

    process_persisted_log_reader(file, &mut scanner)?;
    index.scanner_state = scanner.into_state();
    index.indexed_len = file_len;
    index
        .complete_record_end_offsets
        .extend_from_slice(&new_offsets);

    Ok(new_offsets)
}

fn load_persisted_log_index(log_path: &Path) -> std::io::Result<PersistedLogIndex> {
    let meta = read_log_index_meta(log_path)?;
    let offsets = read_log_index_offsets(log_path)?;
    Ok(PersistedLogIndex::from_meta(meta, offsets))
}

fn read_log_index_meta(log_path: &Path) -> std::io::Result<PersistedLogIndexMeta> {
    let bytes = fs::read(log_index_meta_path(log_path))?;
    serde_json::from_slice(&bytes)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}

fn read_log_index_offsets(log_path: &Path) -> std::io::Result<Vec<u64>> {
    let bytes = fs::read(log_index_offsets_path(log_path))?;
    if bytes.len() % std::mem::size_of::<u64>() != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "log index offsets file has invalid length",
        ));
    }

    Ok(bytes
        .chunks_exact(std::mem::size_of::<u64>())
        .map(|chunk| {
            let mut buf = [0u8; std::mem::size_of::<u64>()];
            buf.copy_from_slice(chunk);
            u64::from_le_bytes(buf)
        })
        .collect())
}

fn write_log_index_offsets(log_path: &Path, offsets: &[u64]) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(offsets.len() * std::mem::size_of::<u64>());
    for offset in offsets {
        bytes.extend_from_slice(&offset.to_le_bytes());
    }

    fs::write(log_index_offsets_path(log_path), bytes)
}

fn append_log_index_offsets(log_path: &Path, offsets: &[u64]) -> std::io::Result<()> {
    if offsets.is_empty() {
        return Ok(());
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_index_offsets_path(log_path))?;
    for offset in offsets {
        file.write_all(&offset.to_le_bytes())?;
    }
    file.flush()
}

fn write_log_index_meta(log_path: &Path, meta: &PersistedLogIndexMeta) -> std::io::Result<()> {
    let path = log_index_meta_path(log_path);
    let temp_path = path.with_extension("meta.tmp");
    let bytes = serde_json::to_vec(meta)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    fs::write(&temp_path, bytes)?;
    match fs::rename(&temp_path, &path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&path);
            match fs::rename(&temp_path, &path) {
                Ok(()) => Ok(()),
                Err(rename_err) => {
                    let _ = fs::remove_file(&temp_path);
                    Err(rename_err)
                }
            }
        }
        Err(err) => {
            let _ = fs::remove_file(&temp_path);
            Err(err)
        }
    }
}

fn log_index_offsets_path(log_path: &Path) -> PathBuf {
    log_path.with_file_name(LOG_INDEX_OFFSETS_FILE)
}

fn log_index_meta_path(log_path: &Path) -> PathBuf {
    log_path.with_file_name(LOG_INDEX_META_FILE)
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

pub fn read_resize_events(session_dir: &Path) -> Result<Vec<LogResize>> {
    let events_path = session_dir.join("events.log");
    let Ok(file) = File::open(events_path) else {
        return Ok(Vec::new());
    };

    let reader = BufReader::new(file);
    let mut events = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if let Some(event) = parse_resize_event(&line) {
            events.push(event);
        }
    }

    Ok(events)
}

pub(super) fn read_relevant_resize_events(
    log_path: &Path,
    start_offset: u64,
    end_offset: u64,
) -> Result<ViewportReplayPlan> {
    let Some(session_dir) = log_path.parent() else {
        return Ok(ViewportReplayPlan::default());
    };

    let mut initial = None;
    let mut resizes = Vec::new();
    for event in read_resize_events(session_dir)? {
        if event.offset <= start_offset {
            initial = Some(event);
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

    Ok(ViewportReplayPlan { initial, resizes })
}

pub(super) fn parse_resize_event(line: &str) -> Option<LogResize> {
    let mut offset = None;
    let mut rows = None;
    let mut cols = None;

    let mut parts = line.split_ascii_whitespace();
    if parts.next()? != "resize" {
        return None;
    }

    for part in parts {
        let (key, value) = part.split_once('=')?;
        match key {
            "offset" => offset = value.parse::<u64>().ok(),
            "rows" => rows = value.parse::<u16>().ok().map(|row| row.max(1)),
            "cols" => cols = value.parse::<u16>().ok().map(|col| col.max(1)),
            _ => {}
        }
    }

    Some(LogResize {
        offset: offset?,
        rows: rows?,
        cols: cols?,
    })
}
