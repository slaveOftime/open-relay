//! Shared log-reading utilities.
//!
//! CLI and HTTP consumers read canonical filtered output from journals.
//! `screen_history` continuously replays recorded geometry for terminal views;
//! `render` also supplies adapters for legacy byte fixtures and resume hints.
//! `index` supplies bounded canonical byte pages.

mod index;
mod render;
mod screen_history;
#[cfg(test)]
mod tests;

use crate::protocol::LogResize;

pub use index::{read_persisted_log_page, split_rendered_log_output};
pub use render::{
    PARSER_COLS, RESUME_FALLBACK_TAIL_LINES, engine_content_rows, finish_render,
    format_history_rows, render_log_session, render_log_session_tail, replay_filtered_tail,
};
pub use screen_history::{
    ScreenSnapshot, collect as collect_screen_history, current as current_recorded_screen,
};

/// Terminal dimensions a caller wants the log replayed at.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewportSize {
    pub rows: u16,
    pub cols: u16,
}

/// The resizes that apply to a span of the log, so replay can reproduce the
/// geometry the output was originally written at.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ViewportReplayPlan {
    pub(super) initial: Option<LogResize>,
    pub(super) resizes: Vec<LogResize>,
}

/// One frame of bytes to replay, tagged with whether it entered the alternate
/// screen (which changes how the parser must be sized).
pub(super) struct RenderBytes<'a> {
    pub(super) frame: &'a [u8],
    pub(super) frame_has_alt_screen: bool,
}

const ESCAPE_BYTE: u8 = 0x1b;

/// The reset sequence appended after rendered output so a caller's terminal is
/// left in a clean state.
pub(super) const OUTPUT_COLOR_RESET_SUFFIX: &[u8] = b"\x1b[0m\x1b[39m\x1b[49m\x1b[?25h";
