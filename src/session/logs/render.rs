//! Replaying raw log bytes through the terminal engine into displayable rows.
//!
//! The log is a raw PTY byte stream, so the only way to know what the user saw
//! is to feed it to a terminal emulator and read the resulting grid. M6-1
//! retired the second (`vt100`) parser: rendering uses the same
//! [`crate::terminal::Terminal`] engine as live sessions, so what `oly logs`
//! shows is exactly what the attached renderer saw.

use std::path::Path;

use crate::error::Result;
use crate::protocol::LogResize;
use crate::terminal::Terminal;

#[cfg(test)]
use super::index::read_tail_bytes;
use super::index::viewport_resize_plan;
use super::{OUTPUT_COLOR_RESET_SUFFIX, RenderBytes, ViewportReplayPlan, ViewportSize};

/// Wide parser column count — prevents any line wrapping inside the engine
/// grid for plain scrollback-style logs.
const PARSER_COLS: u16 = 2000;

/// Fallback viewport height for alt-screen TUIs when no absolute row movement
/// is visible in the retained log tail.
const DEFAULT_ALT_SCREEN_ROWS: u16 = 24;

/// Render a session's persisted output for `oly logs` / the HTTP tail
/// endpoint from the journal-derived filtered stream. Pre-1.0 sessions
/// that only have `output.log` are rejected (M6-2 removed the fallback;
/// see MIGRATION.md).
pub fn render_log_session(
    session_dir: &Path,
    tail: usize,
    keep_color: bool,
    term_cols: u16,
    viewport: Option<ViewportSize>,
) -> Result<(Vec<u8>, Vec<LogResize>)> {
    if !session_dir
        .join(crate::session::journal::JOURNAL_DIR_NAME)
        .is_dir()
    {
        return Err(crate::error::AppError::Protocol(format!(
            "session log in {} uses the pre-0.5 format (output.log) and is no \
             longer readable; export it with a 0.x build first, see MIGRATION.md",
            session_dir.display()
        )));
    }

    // Checkpoint-anchored tail replay (PLAN §5.3): start at the newest
    // anchored checkpoint and walk older anchors only until the replayed
    // suffix covers the requested tail. Cost is bounded by the checkpoint
    // cadence, not by total recording size.
    let anchors = crate::session::replay::replay_anchors(session_dir).unwrap_or_default();
    let mut starts: Vec<u64> = anchors.iter().rev().copied().collect();
    starts.push(0);
    let mut rendered: Option<(Vec<u8>, u64, u64, Vec<LogResize>)> = None;
    for start in starts {
        let (bytes, end) = crate::session::replay::filtered_stream_from(session_dir, start)?;
        let tail_bytes = super::index::tail_window_bytes(&bytes, tail);
        debug_assert_eq!(tail_bytes.end_offset + start, end);
        // start_offset > 0 means the window found enough lines inside the
        // suffix; otherwise widen the replay at an older anchor.
        if tail_bytes.start_offset > 0 || start == 0 {
            let start_offset = start + tail_bytes.start_offset;
            // M6-2: resize history is derived from the journal
            // (append-ordered with output), not the retired events.log.
            // Anchored: only resizes inside the replayed window are derived
            // — a bounded replay that breaks once we're 64 MiB past the
            // window, never a full re-scan of the recording.
            let resizes = crate::session::replay::resize_events_from(session_dir, start_offset)?;
            rendered = Some((tail_bytes.bytes, start_offset, end, resizes));
            break;
        }
    }
    let Some((bytes, start_offset, end_offset, resizes)) = rendered else {
        return Ok((Vec::new(), Vec::new()));
    };
    let viewport_plan = if viewport.is_some() {
        ViewportReplayPlan::default()
    } else {
        viewport_resize_plan(&resizes, start_offset, end_offset)
    };

    Ok((
        render_log_bytes(
            &bytes,
            tail,
            keep_color,
            term_cols,
            viewport,
            &viewport_plan,
        ),
        resizes,
    ))
}

/// Renders a standalone stream file. Used by the transcript golden tests;
/// kept because the byte pipeline is shared with journal rendering.
#[cfg(test)]
pub fn render_log_file(
    log_path: &Path,
    tail: usize,
    keep_color: bool,
    term_cols: u16,
    viewport: Option<ViewportSize>,
) -> Result<(Vec<u8>, Vec<LogResize>)> {
    // Step 1: seek to a position that gives `tail * 2` lines worth of bytes,
    // providing enough context for the replay engine even with heavy escape usage.
    let tail_bytes = read_tail_bytes(log_path, tail)?;
    // A standalone stream file has no journal, hence no resize history.
    let viewport_plan = ViewportReplayPlan::default();
    let _ = viewport.is_some();

    Ok((
        render_log_bytes(
            &tail_bytes.bytes,
            tail,
            keep_color,
            term_cols,
            viewport,
            &viewport_plan,
        ),
        Vec::new(),
    ))
}

/// Render the live session engine's visible screen with the same
/// content-bounded, tail-limited semantics as [`render_screen`]. Plain
/// rows are truncated to `term_cols` characters; styled rows keep the
/// session's PTY width so an SGR sequence is never split.
pub fn render_engine_screen(
    engine: &crate::terminal::Terminal,
    tail: usize,
    keep_color: bool,
    term_cols: u16,
) -> Vec<u8> {
    let content_rows: Vec<Vec<u8>> = if keep_color {
        engine.styled_screen_rows()
    } else {
        engine
            .screen_lines()
            .into_iter()
            .map(|row| truncate_chars(&row, usize::from(term_cols)).into_bytes())
            .collect()
    };
    finish_rows_for_display(content_rows, tail, keep_color)
}

fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn finish_rows_for_display(content_rows: Vec<Vec<u8>>, tail: usize, keep_color: bool) -> Vec<u8> {
    let rows = if let Some((first, last)) = content_bounds(&content_rows) {
        let visible_rows = &content_rows[first..=last];
        let skip = visible_rows.len().saturating_sub(tail);
        visible_rows[skip..].to_vec()
    } else {
        Vec::new()
    };
    format_rows_for_output(&rows, keep_color)
}

/// Shared scrollback-seed formatting for engine rows: trim
/// surrounding blank rows so padding (e.g. blank rows scrolled off by
/// empty prompts) does not crowd out content rows, keep colour, join as
/// `\n`-terminated lines. `None` when nothing has scrolled off yet.
pub fn format_history_rows(rows: Vec<Vec<u8>>) -> Option<Vec<u8>> {
    let (first, last) = content_bounds(&rows)?;
    Some(format_rows_for_output(&rows[first..=last], true))
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
    let mut engine = Terminal::new(
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
    process_bytes_with_resizes(&mut engine, render_bytes.frame, viewport_plan);

    // Plain rows are truncated to `term_cols` characters; styled rows keep
    // the replay width so an SGR sequence is never split (same semantics as
    // [`render_engine_screen`]).
    let content_rows: Vec<Vec<u8>> = if keep_color {
        engine.styled_screen_rows()
    } else {
        engine
            .screen_lines()
            .into_iter()
            .map(|row| truncate_chars(&row, usize::from(term_cols)).into_bytes())
            .collect()
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
    engine: &mut Terminal,
    bytes: &[u8],
    viewport_plan: &ViewportReplayPlan,
) {
    let mut processed = 0usize;

    for resize in &viewport_plan.resizes {
        let resize_offset = resize.offset.min(bytes.len() as u64) as usize;
        if resize_offset > processed {
            engine.feed(&bytes[processed..resize_offset]);
            processed = resize_offset;
        }
        // Replay is side-effect-free: queued engine events (query answers,
        // bells) are drained and discarded, never written anywhere. Replay
        // engines keep no scrollback: only the final visible state matters.
        let _ = engine.drain_events();
        // Engine resize is a non-destructive reflow — no parser rebuild.
        engine.resize(resize.rows, resize.cols);
    }

    if processed < bytes.len() {
        engine.feed(&bytes[processed..]);
        let _ = engine.drain_events();
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
        // Step 2: feed bytes into the terminal engine sized to the inferred
        // frame dimensions, then collect each visible row formatted and
        // trimmed to the terminal width.
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
    trim_styled_row_end(row).is_empty()
}

fn trim_row_end(row: &[u8], keep_color: bool) -> &[u8] {
    if keep_color {
        return trim_styled_row_end(row);
    }

    let end = row
        .iter()
        .rposition(|&byte| !byte.is_ascii_whitespace())
        .map_or(0, |index| index + 1);
    &row[..end]
}

/// Trim trailing blank runs from a styled row: padding spaces and the SGR
/// sequences between them are dropped, so log rows don't carry full-width
/// background-color padding to the replay width. Trailing cells revert to
/// the terminal's default background — the same display behavior the
/// pre-M6 renderer had. Non-ASCII (UTF-8 continuation) bytes always count
/// as content; only CSI sequences (`\x1b[` … final byte) are skipped as
/// styling, which is all the engine's styled rows emit.
fn trim_styled_row_end(row: &[u8]) -> &[u8] {
    let mut last_content_end = 0;
    let mut index = 0;
    while index < row.len() {
        if row[index] == 0x1b && row.get(index + 1) == Some(&b'[') {
            let mut end = index + 2;
            while end < row.len() && !(0x40..=0x7e).contains(&row[end]) {
                end += 1;
            }
            index = (end + 1).min(row.len());
            continue;
        }
        index += 1;
        if !row[index - 1].is_ascii_whitespace() {
            last_content_end = index;
        }
    }
    &row[..last_content_end]
}
