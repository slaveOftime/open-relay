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
    end_offset: u64,
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
    let viewport = if viewport.is_some() {
        viewport
    } else {
        read_latest_viewport_size(log_path, tail_bytes.end_offset)?
    };

    Ok(render_log_bytes(
        &tail_bytes.bytes,
        tail,
        keep_color,
        term_cols,
        viewport,
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

    Ok(TailBytes {
        bytes: buf,
        end_offset: file_size,
    })
}

fn read_latest_viewport_size(log_path: &Path, output_offset: u64) -> Result<Option<ViewportSize>> {
    let Some(session_dir) = log_path.parent() else {
        return Ok(None);
    };

    let events_path = session_dir.join("events.log");
    let Ok(file) = File::open(events_path) else {
        return Ok(None);
    };

    let reader = BufReader::new(file);
    let mut latest = None;

    for line in reader.lines() {
        let line = line?;
        if let Some((offset, viewport)) = parse_resize_event(&line) {
            if offset <= output_offset {
                latest = Some(viewport);
            }
        }
    }

    Ok(latest)
}

fn parse_resize_event(line: &str) -> Option<(u64, ViewportSize)> {
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

    Some((
        offset?,
        ViewportSize {
            rows: rows?,
            cols: cols?,
        },
    ))
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
    bytes: &[u8],
    tail: usize,
    term_cols: u16,
    keep_color: bool,
    viewport: Option<ViewportSize>,
) -> Vec<Vec<u8>> {
    let mut parser = vt100::Parser::new(
        parser_rows(bytes, tail, viewport),
        parser_cols(bytes, term_cols, viewport),
        0,
    );
    parser.process(bytes);

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

fn parser_rows(bytes: &[u8], tail: usize, viewport: Option<ViewportSize>) -> u16 {
    if contains_alt_screen(bytes) {
        viewport
            .map(|size| size.rows)
            .or_else(|| estimate_alt_screen_rows(bytes))
            .unwrap_or(DEFAULT_ALT_SCREEN_ROWS)
    } else {
        tail.clamp(1, u16::MAX as usize) as u16
    }
}

fn parser_cols(bytes: &[u8], term_cols: u16, viewport: Option<ViewportSize>) -> u16 {
    if contains_alt_screen(bytes) {
        viewport
            .map(|size| size.cols)
            .unwrap_or_else(|| term_cols.max(1))
    } else {
        PARSER_COLS
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
) -> Vec<u8> {
    let bytes = latest_render_frame(bytes);

    // Step 2: feed bytes into a vt100 parser sized to (tail rows × 2000 cols),
    // then collect each visible row formatted and trimmed to the terminal width.
    let rows = render_rows(bytes, tail, term_cols, keep_color, viewport);

    format_rows_for_output(&rows, keep_color)
}

fn latest_render_frame(bytes: &[u8]) -> &[u8] {
    if !contains_alt_screen(bytes) {
        return bytes;
    }

    last_subslice(bytes, b"\x1b[H\x1b[2J")
        .or_else(|| last_subslice(bytes, b"\x1b[2J\x1b[H"))
        .map(|start| &bytes[start..])
        .unwrap_or(bytes)
}

fn last_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }

    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

fn format_rows_for_output(rows: &[Vec<u8>], keep_color: bool) -> Vec<u8> {
    let mut out = Vec::new();

    // Find the first non-empty row so we don't print a sea of blank lines when
    // the log is shorter than `tail`.
    let first_content = rows
        .iter()
        .position(|r| !r.is_empty() && r.iter().any(|&b| !b.is_ascii_whitespace()))
        .unwrap_or(0);

    let mut last_content = rows
        .iter()
        .rposition(|r| !r.is_empty() && r.iter().any(|&b| !b.is_ascii_whitespace()))
        .unwrap_or(first_content);

    last_content = trim_repeated_trailing_suffix(rows, first_content, last_content);

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

    row[..row
        .iter()
        .rposition(|&byte| !byte.is_ascii_whitespace())
        .map(|index| index + 1)
        .unwrap_or(0)]
        .as_ref()
}

#[cfg(test)]
mod tests {
    use super::{
        ViewportSize, parse_resize_event, parser_cols, parser_rows, read_latest_viewport_size,
        render_log_bytes, render_log_file,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn expected_fixture(name: &str) -> Vec<u8> {
        fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join(name),
        )
        .expect("read expected fixture")
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

        assert_eq!(output, expected_fixture("output-copilot.expected"));
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

        assert_eq!(output, expected_fixture("output-opencode.expected"));
    }

    #[test]
    fn renders_codex_transcript_exactly() {
        let log_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("output-codex.log");

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
        .expect("render codex output log with color");

        assert_eq!(output, expected_fixture("output-codex.expected"));
    }

    #[test]
    fn keeps_visible_rows_below_cursor() {
        let bytes = b"\x1b[2J\x1b[1;1HTitle\x1b[5;1HOption 1\x1b[6;1HOption 2\x1b[2;1HSearch";

        let output = render_log_bytes(bytes, 10, false, 80, None);
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

        let output = render_log_bytes(bytes, 10, false, 80, None);
        let rendered = String::from_utf8_lossy(&output);

        assert!(!rendered.contains("stale"));
        assert!(rendered.contains("Title"));
        assert!(rendered.contains("Option 1"));
        assert!(rendered.contains("Option 2"));
    }

    #[test]
    fn parses_resize_events() {
        let parsed = parse_resize_event("resize offset=42 rows=37 cols=105");

        assert_eq!(
            parsed,
            Some((
                42,
                ViewportSize {
                    rows: 37,
                    cols: 105,
                },
            ))
        );
    }

    #[test]
    fn reads_latest_viewport_from_events_log() {
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
            b"resize offset=0 rows=24 cols=80\nresize offset=10 rows=37 cols=105\n",
        )
        .expect("write events log");

        let viewport = read_latest_viewport_size(&log_path, 999).expect("read viewport");

        assert_eq!(
            viewport,
            Some(ViewportSize {
                rows: 37,
                cols: 105,
            })
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn persisted_viewport_overrides_alt_screen_fallback_dimensions() {
        let bytes = b"\x1b[?1049h\x1b[20;1Hstale\x1b[HSelect Model";
        let viewport = Some(ViewportSize { rows: 6, cols: 100 });

        assert_eq!(parser_rows(bytes, 10, viewport), 6);
        assert_eq!(parser_cols(bytes, 80, viewport), 100);
        assert_eq!(parser_cols(bytes, 80, None), 80);
    }
}
