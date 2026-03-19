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

pub fn render_log_file(
    log_path: &Path,
    tail: usize,
    keep_color: bool,
    term_cols: u16,
) -> Result<Vec<u8>> {
    // Step 1: seek to a position that gives `tail * 2` lines worth of bytes,
    // providing enough context for the vt100 parser even with heavy escape usage.
    let bytes = read_tail_bytes(log_path, tail)?;

    Ok(render_log_bytes(&bytes, tail, keep_color, term_cols))
}

/// Seek near the end of the log file and read enough bytes to cover `tail * 2`
/// lines (using a generous per-line estimate), returning the raw bytes.
///
/// If the seek position doesn't land at byte 0, the first partial line is
/// dropped to avoid feeding truncated ANSI escape sequences into a downstream
/// parser (which can corrupt subsequent color state).
fn read_tail_bytes(log_path: &Path, tail: usize) -> Result<Vec<u8>> {
    let mut file = File::open(log_path)?;
    let file_size = file.seek(SeekFrom::End(0))?;

    let margin = (tail as u64) * 2 * BYTES_PER_LINE_ESTIMATE;
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
fn render_rows(bytes: &[u8], tail: usize, term_cols: u16, keep_color: bool) -> Vec<Vec<u8>> {
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

fn render_log_bytes(bytes: &[u8], tail: usize, keep_color: bool, term_cols: u16) -> Vec<u8> {
    // Step 2: feed bytes into a vt100 parser sized to (tail rows × 2000 cols),
    // then collect each visible row formatted and trimmed to the terminal width.
    let rows = render_rows(bytes, tail, term_cols, keep_color);

    format_rows_for_output(&rows, keep_color)
}

fn format_rows_for_output(rows: &[Vec<u8>], keep_color: bool) -> Vec<u8> {
    let mut out = Vec::new();

    // Find the first non-empty row so we don't print a sea of blank lines when
    // the log is shorter than `tail`.
    let first_content = rows
        .iter()
        .position(|r| !r.is_empty() && r.iter().any(|&b| !b.is_ascii_whitespace()))
        .unwrap_or(0);

    let last_content = rows
        .iter()
        .rposition(|r| !r.is_empty() && r.iter().any(|&b| !b.is_ascii_whitespace()))
        .unwrap_or(first_content);

    for row in &rows[first_content..=last_content] {
        out.extend_from_slice(trim_row_end(row, keep_color));
        if keep_color {
            out.extend_from_slice(b"\x1b[0m");
        }
        out.push(b'\n');
    }

    if keep_color {
        out.extend_from_slice(b"\x1b[0m\x1b[39m\x1b[49m\x1b[?25h");
    }

    out
}

fn trim_row_end(row: &[u8], keep_color: bool) -> &[u8] {
    if keep_color {
        return row;
    }

    row[..row
        .iter()
        .rposition(|&byte| !byte.is_ascii_whitespace())
        .map(|index| index + 1)
        .unwrap_or(0)]
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::render_log_file;
    use std::path::PathBuf;

    #[test]
    fn renders_copilot_transcript_exactly() {
        let log_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("copilot-output.log");

        let output = render_log_file(&log_path, 40, true, 100)
            .expect("render copilot output log with color");

        let expected = " ~                                                                              gpt-5.4 (high) (1x) \u{1b}[0m\n\u{1b}[2m────────────────────────────────────────────────────────────────────────────────────────────────────\u{1b}[0m\n❯ \u{1b}[7m \u{1b}[mType @ to mention files, # for issues/PRs, / for commands, or ? for shortcuts\u{1b}[0m\n\u{1b}[2m────────────────────────────────────────────────────────────────────────────────────────────────────\u{1b}[0m\n\u{1b}[1mSelect Model\u{1b}[0m\n\u{1b}[0m\nChoose the AI model to use for Copilot CLI. The selected model will be persisted and used for future\u{1b}[0m\n sessions.\u{1b}[0m\n\u{1b}[0m\nSome models are not available. For information on Copilot policies and subscription, visit:\u{1b}[0m\nhttps://github.com/settings/copilot/features\u{1b}[0m\n\u{1b}[0m\n\u{1b}[7m \u{1b}[mSearch models...\u{1b}[0m\n\u{1b}[0m\n  Gemini 3 Pro (Preview) (default)        \u{1b}[90m1x\u{1b}[K\u{1b}[0m\n  \u{1b}[32mGPT-5.4 ✓\u{1b}[31X\u{1b}[31C\u{1b}[90m1x\u{1b}[K\u{1b}[0m\n  GPT-5.3-Codex\u{1b}[27C\u{1b}[90m1x\u{1b}[K\u{1b}[0m\n  GPT-5.2-Codex\u{1b}[27C\u{1b}[90m1x\u{1b}[K\u{1b}[0m\n\u{1b}[36m❯ GPT-5.2\u{1b}[33X\u{1b}[33C\u{1b}[90m1x\u{1b}[K\u{1b}[0m\n  GPT-5.1-Codex-Max\u{1b}[23C\u{1b}[90m1x\u{1b}[K\u{1b}[0m\n  GPT-5.1-Codex\u{1b}[27C\u{1b}[90m1x\u{1b}[K\u{1b}[0m\n  GPT-5.1\u{1b}[33C\u{1b}[90m1x\u{1b}[K\u{1b}[0m\n  GPT-5.4 mini\u{1b}[25C\u{1b}[90m0.33x\u{1b}[K\u{1b}[0m\n  GPT-5.1-Codex-Mini (Preview)\u{1b}[9C\u{1b}[90m0.33x\u{1b}[K\u{1b}[0m\n  GPT-5 mini\u{1b}[30C\u{1b}[90m0x\u{1b}[K\u{1b}[0m\n  GPT-4.1\u{1b}[33C\u{1b}[90m0x\u{1b}[K\u{1b}[0m\n\u{1b}[0m\n\u{1b}[1m↑↓\u{1b}[m to navigate · \u{1b}[1mEnter\u{1b}[m to select · \u{1b}[1mEsc\u{1b}[m to cancel\u{1b}[0m\n\u{1b}[0m\u{1b}[39m\u{1b}[49m\u{1b}[?25h";

        assert_eq!(String::from_utf8_lossy(&output), expected);
    }
}
