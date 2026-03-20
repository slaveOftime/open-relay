//! Shared log-reading utilities.
//!
//! Both the CLI (`oly logs`) and the HTTP `/sessions/{id}/logs` endpoint read
//! persisted `output.log` files from disk.  This module consolidates that logic
//! so every consumer shares the same code path.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::{error::Result, protocol::LogResize};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Fallback record size for raw PTY log pagination when no natural terminal
/// boundary appears for a long stretch of bytes.
const LOG_RECORD_FALLBACK_BYTES: usize = 2048;

const ESCAPE_BYTE: u8 = 0x1b;

/// Wide parser column count — prevents any line wrapping inside the vt100 grid
/// for plain scrollback-style logs.
const PARSER_COLS: u16 = 2000;

/// Fallback viewport height for alt-screen TUIs when no absolute row movement
/// is visible in the retained log tail.
const DEFAULT_ALT_SCREEN_ROWS: u16 = 24;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewportSize {
    pub rows: u16,
    pub cols: u16,
}

struct TailBytes {
    bytes: Vec<u8>,
    start_offset: u64,
    end_offset: u64,
}

struct RenderBytes<'a> {
    frame: &'a [u8],
    frame_has_alt_screen: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ViewportReplayPlan {
    initial: Option<LogResize>,
    resizes: Vec<LogResize>,
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
    let file = File::open(session_dir.join("output.log")).ok()?;
    let mut page = PaginatedLogRecords::new(offset, limit);
    scan_persisted_log_records(file, |record| page.push(record)).ok()?;
    Some(page.finish())
}

#[cfg(test)]
fn split_persisted_log_records(bytes: &[u8]) -> Vec<String> {
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

    scanner.finish();
    Ok(())
}

struct LogRecordScanner<F>
where
    F: FnMut(&[u8]),
{
    current_record: Vec<u8>,
    pending_escape: Vec<u8>,
    on_record: F,
}

impl<F> LogRecordScanner<F>
where
    F: FnMut(&[u8]),
{
    fn new(on_record: F) -> Self {
        Self {
            current_record: Vec::with_capacity(LOG_RECORD_FALLBACK_BYTES),
            pending_escape: Vec::new(),
            on_record,
        }
    }

    fn process_bytes(&mut self, bytes: &[u8]) {
        let mut remaining = bytes;

        while !remaining.is_empty() {
            if !self.pending_escape.is_empty() {
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
                    self.current_record.push(remaining[special_index]);
                    self.flush_current_record();
                }
                ESCAPE_BYTE => self.pending_escape.push(ESCAPE_BYTE),
                _ => unreachable!("special record byte lookup returned unsupported byte"),
            }

            remaining = &remaining[special_index + 1..];
        }
    }

    fn process_pending_escape_bytes(&mut self, bytes: &[u8]) -> usize {
        for (index, &byte) in bytes.iter().enumerate() {
            self.pending_escape.push(byte);

            match ansi_sequence_status(&self.pending_escape) {
                AnsiSequenceStatus::Incomplete => {}
                AnsiSequenceStatus::Complete => {
                    let is_boundary = is_record_boundary_sequence(&self.pending_escape);
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
            let available = LOG_RECORD_FALLBACK_BYTES.saturating_sub(self.current_record.len());
            if available == 0 {
                self.flush_current_record();
                continue;
            }

            if bytes.len() <= available {
                self.current_record.extend_from_slice(bytes);
                return;
            }

            let split_at = utf8_boundary_at_or_before(bytes, available).max(1);
            self.current_record.extend_from_slice(&bytes[..split_at]);
            self.flush_current_record();
            bytes = &bytes[split_at..];
        }
    }

    fn finish(&mut self) {
        if !self.pending_escape.is_empty() {
            self.current_record.extend_from_slice(&self.pending_escape);
            self.pending_escape.clear();
        }

        self.flush_current_record();
    }

    fn flush_current_record(&mut self) {
        if self.current_record.is_empty() {
            return;
        }

        (self.on_record)(&self.current_record);
        self.current_record.clear();
    }

    fn flush_pending_escape(&mut self, is_boundary: bool) {
        if is_boundary && !self.current_record.is_empty() {
            self.flush_current_record();
        }

        self.current_record.extend_from_slice(&self.pending_escape);
        self.pending_escape.clear();
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

pub fn render_log_file(
    log_path: &Path,
    tail: usize,
    keep_color: bool,
    term_cols: u16,
    viewport: Option<ViewportSize>,
) -> Result<Vec<u8>> {
    // Step 1: seek to a position that gives `tail * 2` lines worth of bytes,
    // providing enough context for the vt100 parser even with heavy escape usage.
    let tail_bytes = read_tail_bytes(log_path, tail)?;
    let viewport_plan = if viewport.is_some() {
        ViewportReplayPlan::default()
    } else {
        read_relevant_resize_events(log_path, tail_bytes.start_offset, tail_bytes.end_offset)?
    };

    Ok(render_log_bytes(
        &tail_bytes.bytes,
        tail,
        keep_color,
        term_cols,
        viewport,
        &viewport_plan,
    ))
}

/// Seek near the end of the log file and read enough bytes to cover `tail * 2`
/// lines (using a generous per-line estimate), returning the raw bytes.
///
/// If the seek position doesn't land at byte 0, the first partial line is
/// dropped to avoid feeding truncated ANSI escape sequences into a downstream
/// parser (which can corrupt subsequent color state).
fn read_tail_bytes(log_path: &Path, tail: usize) -> Result<TailBytes> {
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

fn read_relevant_resize_events(
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

fn parse_resize_event(line: &str) -> Option<LogResize> {
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

// ---------------------------------------------------------------------------
// vt100-based rendering (CLI output)
// ---------------------------------------------------------------------------

/// Parse raw log bytes through a virtual terminal and collect
/// the last `tail` visible ANSI-formatted row byte vectors, each trimmed to
/// `term_cols`.
///
/// Do not use the cursor row as the content boundary. Full-screen TUIs often
/// keep the cursor in an input field near the top of the screen while painting
/// additional visible rows below it. Trailing blank rows are trimmed later by
/// `format_rows_for_output`.
///
/// For alternate-screen TUIs, `tail` is not a valid parser height. The parser
/// must approximate the PTY viewport height, otherwise absolute cursor writes
/// can leave stale off-screen rows visible in an oversized virtual screen.
fn render_rows(
    render_bytes: &RenderBytes<'_>,
    tail: usize,
    term_cols: u16,
    keep_color: bool,
    viewport: Option<ViewportSize>,
    viewport_plan: &ViewportReplayPlan,
) -> Vec<Vec<u8>> {
    let mut parser = vt100::Parser::new(
        parser_rows(
            render_bytes.frame,
            render_bytes.frame_has_alt_screen,
            tail,
            viewport,
            viewport_plan,
        ),
        parser_cols(
            render_bytes.frame_has_alt_screen,
            term_cols,
            viewport,
            viewport_plan,
        ),
        0,
    );
    process_bytes_with_resizes(&mut parser, render_bytes.frame, viewport_plan);

    let screen = parser.screen();

    let content_rows: Vec<Vec<u8>> = if keep_color {
        screen.rows_formatted(0, term_cols).collect()
    } else {
        screen.rows(0, term_cols).map(|s| s.into_bytes()).collect()
    };

    // Take the last `tail` rows from the content region.
    let skip = content_rows.len().saturating_sub(tail);
    content_rows.into_iter().skip(skip).collect()
}

fn parser_rows(
    bytes: &[u8],
    has_alt_screen: bool,
    tail: usize,
    viewport: Option<ViewportSize>,
    viewport_plan: &ViewportReplayPlan,
) -> u16 {
    if has_alt_screen {
        viewport_plan
            .initial
            .as_ref()
            .map(|size| size.rows)
            .or_else(|| viewport.map(|size| size.rows))
            .or_else(|| viewport_plan.resizes.first().map(|size| size.rows))
            .or_else(|| estimate_alt_screen_rows(bytes))
            .unwrap_or(DEFAULT_ALT_SCREEN_ROWS)
    } else {
        tail.clamp(1, u16::MAX as usize) as u16
    }
}

fn parser_cols(
    has_alt_screen: bool,
    term_cols: u16,
    viewport: Option<ViewportSize>,
    viewport_plan: &ViewportReplayPlan,
) -> u16 {
    if has_alt_screen {
        viewport_plan
            .initial
            .as_ref()
            .map(|size| size.cols)
            .or_else(|| viewport.map(|size| size.cols))
            .or_else(|| viewport_plan.resizes.first().map(|size| size.cols))
            .unwrap_or_else(|| term_cols.max(1))
    } else {
        PARSER_COLS
    }
}

fn process_bytes_with_resizes(
    parser: &mut vt100::Parser,
    bytes: &[u8],
    viewport_plan: &ViewportReplayPlan,
) {
    let mut processed = 0usize;

    for resize in &viewport_plan.resizes {
        let resize_offset = resize.offset.min(bytes.len() as u64) as usize;
        if resize_offset > processed {
            parser.process(&bytes[processed..resize_offset]);
            processed = resize_offset;
        }
        parser.screen_mut().set_size(resize.rows, resize.cols);
    }

    if processed < bytes.len() {
        parser.process(&bytes[processed..]);
    }
}

fn contains_alt_screen(bytes: &[u8]) -> bool {
    bytes.windows(8).any(|window| {
        matches!(
            window,
            b"\x1b[?1049h" | b"\x1b[?1049l" | b"\x1b[?1047h" | b"\x1b[?1047l"
        )
    })
}

fn estimate_alt_screen_rows(bytes: &[u8]) -> Option<u16> {
    let mut max_row = 0u16;
    let mut index = 0usize;

    while index + 2 < bytes.len() {
        if bytes[index] != 0x1b || bytes[index + 1] != b'[' {
            index += 1;
            continue;
        }

        let sequence = &bytes[index + 2..];
        let Some(final_offset) = sequence
            .iter()
            .position(|byte| (0x40..=0x7e).contains(byte))
        else {
            break;
        };
        let final_byte = sequence[final_offset];
        let params = &sequence[..final_offset];

        if let Some(row) = extract_absolute_row(params, final_byte) {
            max_row = max_row.max(row);
        }

        index += 2 + final_offset + 1;
    }

    if max_row == 0 {
        None
    } else {
        Some(max_row.max(DEFAULT_ALT_SCREEN_ROWS))
    }
}

fn extract_absolute_row(params: &[u8], final_byte: u8) -> Option<u16> {
    match final_byte {
        b'H' | b'f' => {
            let row = params.split(|byte| *byte == b';').next()?;
            parse_csi_number(row)
        }
        b'd' => parse_csi_number(params),
        _ => None,
    }
}

fn parse_csi_number(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() || bytes[0] == b'?' {
        return None;
    }

    let digits_end = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits_end == 0 {
        return None;
    }

    std::str::from_utf8(&bytes[..digits_end])
        .ok()?
        .parse::<u16>()
        .ok()
}

fn render_log_bytes(
    bytes: &[u8],
    tail: usize,
    keep_color: bool,
    term_cols: u16,
    viewport: Option<ViewportSize>,
    viewport_plan: &ViewportReplayPlan,
) -> Vec<u8> {
    let mut fallback_output = None;

    for render_bytes in prepare_render_bytes(bytes) {
        // Step 2: feed bytes into a vt100 parser sized to the inferred frame
        // dimensions, then collect each visible row formatted and trimmed to
        // the terminal width.
        let rows = render_rows(
            &render_bytes,
            tail,
            term_cols,
            keep_color,
            viewport,
            viewport_plan,
        );
        let output = format_rows_for_output(&rows, keep_color);

        if fallback_output.is_none() {
            fallback_output = Some(output.clone());
        }

        if content_bounds(&rows).is_some() {
            return output;
        }
    }

    fallback_output.unwrap_or_else(|| format_rows_for_output(&[], keep_color))
}

fn prepare_render_bytes(bytes: &[u8]) -> Vec<RenderBytes<'_>> {
    let has_alt_screen = contains_alt_screen(bytes);
    if !has_alt_screen {
        return vec![RenderBytes {
            frame: bytes,
            frame_has_alt_screen: false,
        }];
    }

    frame_segments(bytes)
        .into_iter()
        .map(|frame| RenderBytes {
            frame,
            frame_has_alt_screen: contains_alt_screen(frame),
        })
        .collect()
}

fn frame_segments(bytes: &[u8]) -> Vec<&[u8]> {
    let starts = frame_start_offsets(bytes);
    if starts.is_empty() {
        return vec![bytes];
    }

    let mut frames = Vec::with_capacity(starts.len());
    for (index, &start) in starts.iter().enumerate().rev() {
        let end = starts.get(index + 1).copied().unwrap_or(bytes.len());
        if start < end {
            frames.push(&bytes[start..end]);
        }
    }

    if frames.is_empty() {
        vec![bytes]
    } else {
        frames
    }
}

fn frame_start_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    for needle in [
        b"\x1b[H\x1b[2J".as_slice(),
        b"\x1b[2J\x1b[H".as_slice(),
        b"\x1b[?1049h".as_slice(),
        b"\x1b[?1047h".as_slice(),
        b"\x1b[?1049l".as_slice(),
        b"\x1b[?1047l".as_slice(),
    ] {
        extend_subslice_positions(bytes, needle, &mut starts);
    }

    starts.sort_unstable();
    starts.dedup();
    starts
}

fn extend_subslice_positions(haystack: &[u8], needle: &[u8], starts: &mut Vec<usize>) {
    if needle.is_empty() || haystack.len() < needle.len() {
        return;
    }

    starts.extend(
        haystack
            .windows(needle.len())
            .enumerate()
            .filter_map(|(index, window)| (window == needle).then_some(index)),
    );
}

fn format_rows_for_output(rows: &[Vec<u8>], keep_color: bool) -> Vec<u8> {
    let mut out = Vec::new();

    if rows.is_empty() {
        append_color_reset(&mut out, keep_color);
        return out;
    }

    // Find the first non-empty row so we don't print a sea of blank lines when
    // the log is shorter than `tail`.
    let (first_content, mut last_content) = content_bounds(rows).unwrap_or((0, 0));

    last_content = trim_repeated_trailing_suffix(rows, first_content, last_content);

    for row in &rows[first_content..=last_content] {
        out.extend_from_slice(trim_row_end(row, keep_color));
        if keep_color {
            out.extend_from_slice(b"\x1b[0m");
        }
        out.push(b'\n');
    }

    append_color_reset(&mut out, keep_color);

    out
}

fn content_bounds(rows: &[Vec<u8>]) -> Option<(usize, usize)> {
    let first = rows.iter().position(|row| !row_is_blank(row))?;
    let last = rows
        .iter()
        .rposition(|row| !row_is_blank(row))
        .unwrap_or(first);
    Some((first, last))
}

fn append_color_reset(out: &mut Vec<u8>, keep_color: bool) {
    if keep_color {
        out.extend_from_slice(b"\x1b[0m\x1b[39m\x1b[49m\x1b[?25h");
    }
}

fn trim_repeated_trailing_suffix(
    rows: &[Vec<u8>],
    first_content: usize,
    last_content: usize,
) -> usize {
    for split in (first_content + 1)..=last_content {
        let split = last_content - (split - (first_content + 1));
        if !row_is_blank(&rows[split]) {
            continue;
        }

        let suffix_start = split + 1;
        if suffix_start > last_content {
            continue;
        }

        let suffix = &rows[suffix_start..=last_content];
        if suffix.len() < 2 {
            continue;
        }

        for candidate_start in first_content..split {
            let candidate_end = candidate_start + suffix.len();
            if candidate_end > split {
                break;
            }

            if rows[candidate_start..candidate_end] == *suffix {
                return split.saturating_sub(1);
            }
        }
    }

    last_content
}

fn row_is_blank(row: &[u8]) -> bool {
    row.is_empty() || row.iter().all(|byte| byte.is_ascii_whitespace())
}

fn trim_row_end(row: &[u8], keep_color: bool) -> &[u8] {
    if keep_color {
        return row;
    }

    let end = row
        .iter()
        .rposition(|&byte| !byte.is_ascii_whitespace())
        .map_or(0, |index| index + 1);
    &row[..end]
}

#[cfg(test)]
mod tests {
    use super::{
        ViewportReplayPlan, ViewportSize, parse_resize_event, parser_cols, parser_rows,
        read_relevant_resize_events, read_resize_events, render_log_bytes, render_log_file,
        split_persisted_log_records,
    };
    use crate::protocol::LogResize;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn expected_fixture(name: &str) -> Vec<u8> {
        let name = fixture_name(name);
        let bytes = fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join(name),
        )
        .expect("read expected fixture");

        normalize_fixture_line_endings(&bytes)
    }

    fn assert_fixture_or_update(name: &str, output: &[u8]) {
        let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join(fixture_name(name));
        if std::env::var_os("OLY_UPDATE_LOG_FIXTURES").is_some() {
            fs::write(&fixture_path, output).expect("write updated log fixture");
        }

        assert_eq!(output, expected_fixture(name));
    }

    fn fixture_name(name: &str) -> &str {
        #[cfg(windows)]
        if name == "output-copilot.expected" {
            return "output-copilot.expected.windows";
        }

        name
    }

    fn normalize_fixture_line_endings(bytes: &[u8]) -> Vec<u8> {
        let mut normalized = Vec::with_capacity(bytes.len());
        let mut index = 0;

        while index < bytes.len() {
            if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
                normalized.push(b'\n');
                index += 2;
                continue;
            }

            normalized.push(bytes[index]);
            index += 1;
        }

        normalized
    }

    fn empty_plan() -> ViewportReplayPlan {
        ViewportReplayPlan::default()
    }

    #[test]
    fn renders_copilot_transcript_exactly() {
        let log_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("output-copilot.log");

        let output = render_log_file(
            &log_path,
            40,
            true,
            100,
            Some(ViewportSize {
                rows: 37,
                cols: 105,
            }),
        )
        .expect("render copilot output log with color");

        assert_fixture_or_update("output-copilot.expected", &output);
    }

    #[test]
    fn renders_opencode_transcript_exactly() {
        let log_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("output-opencode.log");

        let output = render_log_file(
            &log_path,
            40,
            true,
            100,
            Some(ViewportSize {
                rows: 37,
                cols: 105,
            }),
        )
        .expect("render opencode output log with color");

        assert_fixture_or_update("output-opencode.expected", &output);
    }

    #[test]
    fn keeps_visible_rows_below_cursor() {
        let bytes = b"\x1b[2J\x1b[1;1HTitle\x1b[5;1HOption 1\x1b[6;1HOption 2\x1b[2;1HSearch";

        let output = render_log_bytes(bytes, 10, false, 80, None, &empty_plan());
        let rendered = String::from_utf8_lossy(&output);

        assert!(rendered.contains("Title"));
        assert!(rendered.contains("Search"));
        assert!(rendered.contains("Option 1"));
        assert!(rendered.contains("Option 2"));
    }

    #[test]
    fn drops_stale_alt_screen_content_before_latest_redraw() {
        let bytes = concat!(
            "\x1b[?1049h",
            "\x1b[20;1Hstale",
            "\x1b[2J\x1b[HTitle",
            "\x1b[5;1HOption 1",
            "\x1b[6;1HOption 2",
            "\x1b[2;1HSearch"
        )
        .as_bytes();

        let output = render_log_bytes(bytes, 10, false, 80, None, &empty_plan());
        let rendered = String::from_utf8_lossy(&output);

        assert!(!rendered.contains("stale"));
        assert!(rendered.contains("Title"));
        assert!(rendered.contains("Option 1"));
        assert!(rendered.contains("Option 2"));
    }

    #[test]
    fn falls_back_to_previous_non_empty_alt_screen_frame() {
        let bytes = concat!(
            "\x1b[?1049h",
            "\x1b[2J\x1b[HTitle",
            "\x1b[2;1HSearch",
            "\x1b[H\x1b[2J"
        )
        .as_bytes();

        let output = render_log_bytes(bytes, 10, false, 80, None, &empty_plan());
        let rendered = String::from_utf8_lossy(&output);

        assert!(rendered.contains("Title"));
        assert!(rendered.contains("Search"));
    }

    #[test]
    fn falls_back_when_alt_screen_teardown_clears_final_output() {
        let bytes = concat!("\x1b[?1049h", "\x1b[2J\x1b[HMenu", "\x1b[?1049l").as_bytes();

        let output = render_log_bytes(bytes, 10, false, 80, None, &empty_plan());
        let rendered = String::from_utf8_lossy(&output);

        assert!(rendered.contains("Menu"));
    }

    #[test]
    fn parses_resize_events() {
        let parsed = parse_resize_event("resize offset=42 rows=37 cols=105");

        assert_eq!(
            parsed,
            Some(LogResize {
                offset: 42,
                rows: 37,
                cols: 105,
            })
        );
    }

    #[test]
    fn reads_all_resize_events_from_events_log() {
        let temp_dir = std::env::temp_dir().join(format!(
            "oly-log-render-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));

        fs::create_dir_all(&temp_dir).expect("create temp dir");

        let log_path = temp_dir.join("output.log");
        let events_path = temp_dir.join("events.log");

        fs::write(&log_path, b"placeholder").expect("write output log");
        fs::write(
            &events_path,
            b"resize offset=0 rows=24 cols=80\nresize offset=10 rows=30 cols=90\nresize offset=20 rows=37 cols=105\n",
        )
        .expect("write events log");

        let resizes = read_resize_events(&temp_dir).expect("read resizes");

        assert_eq!(
            resizes,
            vec![
                LogResize {
                    offset: 0,
                    rows: 24,
                    cols: 80,
                },
                LogResize {
                    offset: 10,
                    rows: 30,
                    cols: 90,
                },
                LogResize {
                    offset: 20,
                    rows: 37,
                    cols: 105,
                },
            ]
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn keeps_last_resize_before_tail_and_future_resizes() {
        let temp_dir = std::env::temp_dir().join(format!(
            "oly-log-render-plan-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));

        fs::create_dir_all(&temp_dir).expect("create temp dir");
        let log_path = temp_dir.join("output.log");
        let events_path = temp_dir.join("events.log");
        fs::write(&log_path, b"placeholder").expect("write output log");
        fs::write(
            &events_path,
            b"resize offset=0 rows=24 cols=80\nresize offset=100 rows=30 cols=90\nresize offset=140 rows=37 cols=105\nresize offset=220 rows=50 cols=140\n",
        )
        .expect("write events log");

        let plan = read_relevant_resize_events(&log_path, 120, 200).expect("read relevant resizes");

        assert_eq!(
            plan,
            ViewportReplayPlan {
                initial: Some(LogResize {
                    offset: 100,
                    rows: 30,
                    cols: 90,
                }),
                resizes: vec![LogResize {
                    offset: 20,
                    rows: 37,
                    cols: 105,
                }],
            }
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn applies_resize_history_during_alt_screen_replay() {
        let bytes = b"\x1b[?1049h\x1b[2J\x1b[H12345";
        let output = render_log_bytes(
            bytes,
            4,
            false,
            10,
            None,
            &ViewportReplayPlan {
                initial: Some(LogResize {
                    offset: 0,
                    rows: 2,
                    cols: 4,
                }),
                resizes: vec![],
            },
        );
        let rendered = String::from_utf8_lossy(&output);

        assert!(rendered.contains("1234"));
        assert!(rendered.contains("5"));
    }

    #[test]
    fn persisted_viewport_overrides_alt_screen_fallback_dimensions() {
        let bytes = b"\x1b[?1049h\x1b[20;1Hstale\x1b[HSelect Model";
        let viewport = Some(ViewportSize { rows: 6, cols: 100 });

        assert_eq!(parser_rows(bytes, true, 10, viewport, &empty_plan()), 6);
        assert_eq!(parser_cols(true, 80, viewport, &empty_plan()), 100);
        assert_eq!(parser_cols(true, 80, None, &empty_plan()), 80);
    }

    #[test]
    fn splits_persisted_logs_on_terminal_boundaries_without_newlines() {
        let records = split_persisted_log_records(
            b"\x1b[2J\x1b[HTitle\x1b[5;1HOption 1\x1b[6;1HOption 2\x1b[2;1HSearch",
        );

        assert_eq!(
            records,
            vec![
                "\x1b[2J".to_string(),
                "\x1b[HTitle".to_string(),
                "\x1b[5;1HOption 1".to_string(),
                "\x1b[6;1HOption 2".to_string(),
                "\x1b[2;1HSearch".to_string(),
            ]
        );
    }

    #[test]
    fn splits_persisted_logs_by_fallback_size_when_no_boundaries_exist() {
        let bytes = vec![b'a'; super::LOG_RECORD_FALLBACK_BYTES + 17];
        let records = split_persisted_log_records(&bytes);

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].len(), super::LOG_RECORD_FALLBACK_BYTES);
        assert_eq!(records[1].len(), 17);
    }
}
