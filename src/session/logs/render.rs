//! Replaying raw log bytes through a `vt100` parser into displayable rows.
//!
//! The log is a raw PTY byte stream, so the only way to know what the user saw
//! is to feed it to a terminal emulator and read the resulting grid.

use std::path::Path;

use crate::error::Result;

use super::super::screen::safe_resize_parser;
use super::index::{read_relevant_resize_events, read_tail_bytes};
use super::{OUTPUT_COLOR_RESET_SUFFIX, RenderBytes, ViewportReplayPlan, ViewportSize};

/// Wide parser column count — prevents any line wrapping inside the vt100 grid
/// for plain scrollback-style logs.
const PARSER_COLS: u16 = 2000;

/// Fallback viewport height for alt-screen TUIs when no absolute row movement
/// is visible in the retained log tail.
const DEFAULT_ALT_SCREEN_ROWS: u16 = 24;

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

pub fn render_screen(
    parser: &vt100::Parser,
    tail: usize,
    keep_color: bool,
    term_cols: u16,
) -> Vec<u8> {
    let screen = parser.screen();
    let content_rows: Vec<Vec<u8>> = if keep_color {
        screen.rows_formatted(0, term_cols).collect()
    } else {
        screen
            .rows(0, term_cols)
            .map(|row| row.into_bytes())
            .collect()
    };
    let rows = if let Some((first, last)) = content_bounds(&content_rows) {
        let visible_rows = &content_rows[first..=last];
        let skip = visible_rows.len().saturating_sub(tail);
        visible_rows[skip..].to_vec()
    } else {
        Vec::new()
    };
    format_rows_for_output(&rows, keep_color)
}

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

pub(super) fn parser_rows(
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

pub(super) fn parser_cols(
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
        safe_resize_parser(parser, resize.rows, resize.cols);
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

pub(super) fn render_log_bytes(
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
        out.extend_from_slice(OUTPUT_COLOR_RESET_SUFFIX);
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
