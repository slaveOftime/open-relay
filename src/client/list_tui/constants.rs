use ratatui::style::Color;
use std::time::Duration;
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_millis(250);
pub(crate) const REFRESH_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(16);
pub(crate) const REDRAW_INTERVAL: Duration = Duration::from_millis(250);
/// Frame cadence while visual effects are running (~30 fps).
pub(crate) const ANIMATION_REDRAW_INTERVAL: Duration = Duration::from_millis(33);
/// The background tint a waiting session's row pulses toward: a dark amber
/// that keeps every status/foreground colour readable on top of it.
pub(crate) const ATTENTION_PULSE_BG: Color = Color::Rgb(90, 62, 4);
/// A selected waiting row pulses toward this blend of the selection band and
/// the attention tint, so the pulse stays visible without hiding that the
/// row is selected.
pub(crate) const ATTENTION_PULSE_BG_SELECTED: Color = Color::Rgb(58, 59, 38);
pub(crate) const RATE_HISTORY_LEN: usize = 30;
pub(crate) const COMPACT_SPARKLINE_WIDTH: usize = 3;
pub(crate) const SPARKLINE_WIDTH: usize = 5;
pub(crate) const INACTIVE_LOG_TAIL_LINES: usize = 1000;
pub(crate) const STOP_GRACE_SECONDS: u64 = 15;
pub(crate) const SPARK_BLOCKS: &[char] = &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
// `pub(crate)` so the Windows crash handler in `crate::client::crash` can
// restore the terminal on unhandled exceptions. Visibility is the minimum
// needed by the cross-module references introduced in PLAN2 S1.3.
pub(crate) const TUI_RESTORE_BYTES: &[u8] = b"\x1b[?1049l\x1b[?2026l\x1b[0m\x1b[?25h\x1b[0 q\
    \x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?2004l";
/// Window title shown while the interactive session list owns the terminal.
pub(crate) const LIST_WINDOW_TITLE: &str = "oly sessions";
/// XTWINOPS 22;0 pushes the current icon + window title onto the terminal's
/// title stack (same scheme as `terminal_guards`), so teardown can restore
/// whatever the surrounding shell had set.
pub(crate) const TITLE_SAVE_BYTES: &[u8] = b"\x1b[22;0t";
/// XTWINOPS 23;0 pops the title saved by `TITLE_SAVE_BYTES`.
// `pub(crate)` so the Windows crash handler in `crate::client::crash` can
// restore the terminal on unhandled exceptions. Visibility is the minimum
// needed by the cross-module references introduced in PLAN2 S1.3.
pub(crate) const TITLE_RESTORE_BYTES: &[u8] = b"\x1b[23;0t";
pub(crate) const CLONE_DIALOG_HELP: &str =
    " Quotes group words · ←/→ cursor · Tab/Shift+Tab · Space toggle · Enter create · Esc cancel";
pub(crate) const UPDATE_DIALOG_HELP: &str =
    " Quotes group words · Tab/Shift+Tab · Space toggle · Enter save · Esc cancel";
pub(crate) const REMOVE_DIALOG_HELP: &str = " Enter/Y remove · Esc/N cancel ";
/// Width of the label column in the clone/update dialogs.
pub(crate) const DIALOG_LABEL_WIDTH: usize = 15;
/// Background of the active field's value, giving it an "input box" look.
pub(crate) const DIALOG_FIELD_BG: Color = Color::Rgb(38, 44, 54);

/// Background used for a selected session row while preserving semantic foreground colors.
pub(crate) const SELECTED_ROW_BG: ratatui::style::Color = ratatui::style::Color::Rgb(25, 55, 72);
