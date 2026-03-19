//! Shared log-reading utilities.
//!
//! Both the CLI (`oly logs`) and the HTTP `/sessions/{id}/logs` endpoint read
//! persisted `output.log` files from disk.  This module consolidates that logic
//! so every consumer shares the same code path.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::Result;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Generous per-line byte budget that accounts for ANSI escape sequences.
const BYTES_PER_LINE_ESTIMATE: u64 = 256;

/// Wide parser column count — prevents any line wrapping inside the vt100 grid.
const PARSER_COLS: u16 = 2000;

// ---------------------------------------------------------------------------
// Disk-based reading
// ---------------------------------------------------------------------------

/// Seek near the end of the log file and read enough bytes to cover `tail * 2`
/// lines (using a generous per-line estimate), returning the raw bytes.
///
/// If the seek position doesn't land at byte 0, the first partial line is
/// dropped to avoid feeding truncated ANSI escape sequences into a downstream
/// parser (which can corrupt subsequent color state).
pub fn read_tail_bytes(log_path: &Path, tail: usize) -> Result<Vec<u8>> {
    let mut file = File::open(log_path)?;
    let file_size = file.seek(SeekFrom::End(0))?;

    let margin = ((tail as u64) * 2).max(200) * BYTES_PER_LINE_ESTIMATE;
    let start = file_size.saturating_sub(margin);

    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((file_size - start) as usize);
    file.read_to_end(&mut buf)?;

    // Drop the first partial line when we didn't start at byte 0.
    if start > 0 {
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            buf.drain(..=pos);
        }
    }

    Ok(buf)
}

/// Read a page of lines from a persisted `output.log`.
///
/// Returns `(lines, total_line_count)` or `None` if the file can't be opened.
/// Lines are returned with their trailing newline intact.
pub fn read_persisted_log_page(
    session_dir: &Path,
    offset: usize,
    limit: usize,
) -> Option<(Vec<String>, usize)> {
    let file = File::open(session_dir.join("output.log")).ok()?;
    let mut reader = BufReader::new(file);

    let end = offset.saturating_add(limit);
    let mut total = 0usize;
    let mut lines = Vec::with_capacity(limit);

    loop {
        let mut buf = String::new();
        let bytes_read = reader.read_line(&mut buf).ok()?;
        if bytes_read == 0 {
            break;
        }

        if total >= offset && total < end {
            lines.push(buf);
        }
        total += 1;
    }

    Some((lines, total))
}

// ---------------------------------------------------------------------------
// vt100-based rendering (CLI output)
// ---------------------------------------------------------------------------

/// Parse raw log bytes through a virtual terminal of `tail` rows and collect
/// the last `tail` visible ANSI-formatted row byte vectors, each trimmed to
/// `term_cols`.
///
/// The cursor row is used as the upper bound of real content so that trailing
/// blank rows (present when the log has fewer lines than `tail`) are excluded
/// before taking the final `tail` rows.
pub fn render_rows(bytes: &[u8], tail: usize, term_cols: u16, keep_color: bool) -> Vec<Vec<u8>> {
    let mut parser = vt100::Parser::new(tail as u16, PARSER_COLS, 0);
    parser.process(bytes);

    let screen = parser.screen();

    // cursor_position() returns (row, col); the cursor row is the last row
    // that received output.  Rows beyond it are blank and should not count.
    let cursor_row = screen.cursor_position().0 as usize;

    let content_rows: Vec<Vec<u8>> = if keep_color {
        screen
            .rows_formatted(0, term_cols)
            .take(cursor_row + 1)
            .collect()
    } else {
        screen
            .rows(0, term_cols)
            .take(cursor_row + 1)
            .map(|s| s.into_bytes())
            .collect()
    };

    // Take the last `tail` rows from the content region.
    let skip = content_rows.len().saturating_sub(tail);
    content_rows.into_iter().skip(skip).collect()
}
