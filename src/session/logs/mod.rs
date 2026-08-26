//! Shared log-reading utilities.
//!
//! Both the CLI (`oly logs`) and the HTTP `/sessions/{id}/logs` endpoint read
//! persisted `output.log` files from disk. This module consolidates that logic
//! so every consumer shares the same code path.
//!
//! It splits into two halves that meet only at the types declared here:
//!
//! - [`index`] turns an `output.log` into addressable records: boundary
//!   detection, the on-disk `output.log.idx` offset index, and pagination.
//! - [`render`] replays raw bytes through a `vt100` parser to produce the rows
//!   a human sees.

mod index;
mod render;
#[cfg(test)]
mod tests;

use crate::protocol::LogResize;

pub use index::{read_persisted_log_page, read_resize_events, split_rendered_log_output};
pub use render::{render_log_file, render_screen};

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
