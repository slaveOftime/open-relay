// ---------------------------------------------------------------------------
// RawModeGuard - RAII for terminal raw mode
// ---------------------------------------------------------------------------

use std::io::Write;

use crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste},
    execute, terminal,
};

use crate::error::{AppError, Result};

fn terminal_tab_state_save_bytes() -> &'static [u8] {
    // XTWINOPS 22;0 saves icon + window title on the terminal's title stack.
    b"\x1b[22;0t"
}

fn terminal_tab_state_restore_bytes() -> &'static [u8] {
    // XTWINOPS 23;0 restores icon + window title from the title stack.
    // OSC 9;4;0;0 clears any forwarded progress indicator. There is no
    // portable query/restore sequence for terminal progress state.
    b"\x1b[23;0t\x1b]9;4;0;0\x07"
}

/// Unconditional terminal normalisation applied on every teardown (detach,
/// natural session end, or the process crashing and unwinding through this
/// guard's `Drop`).  The attached process may have entered its own alternate
/// screen, changed cursor-key mode, enabled mouse tracking, left a
/// synchronized-output frame open, or forwarded a window title / icon /
/// progress indicator (see `passthrough_signals` in the attach client) that
/// must not leak into the parent shell.  We undo all of that:
///
///  \x1b[?1049l  - leave alternate screen (no-op if already on main). For TUI
///                 children this restores the main screen; for non-TUI
///                 children (REPLs, shells) this is a no-op and their output
///                 stays in scrollback.
///  \x1b[?2026l  - disable synchronized output (DECSET 2026). The Windows
///                 attach renderer wraps repaints in 2026h/2026l so the
///                 terminal buffers redraw until the matching 2026l arrives;
///                 a crash or detach that lands between the two would
///                 otherwise leave the terminal frozen forever on stale
///                 content. Also undoes synchronized output the attached
///                 process itself may have left enabled.
///  \x1b[!p      - DECSTR soft terminal reset (resets DECCKM, DECOM, DECAWM,
///                 scroll region, etc. without clearing screen)
///  \x1b[0m      - SGR reset (colors / bold / etc.)
///  \x1b[?25h    - ensure cursor is visible
///  \x1b[0 q     - reset cursor style to terminal default (restores
///                 blinking); DECSCUSR with param 0
///  \x1b[?1000l .. \x1b[?2004l  - disable mouse and bracketed-paste modes the
///                 app may have enabled (belt-and-suspenders alongside
///                 crossterm's DisableBracketedPaste below)
///  \x1b[H\x1b[2J - home cursor then erase entire display.  On modern
///                 terminals (VTE, xterm, kitty, Windows Terminal) ED 2
///                 pushes the visible content into scrollback, so session
///                 output remains accessible via scroll-up.  This gives the
///                 post-detach status message and shell prompt a clean
///                 screen.  For TUI children, \x1b[?1049l already restored
///                 the main screen, so this clears any leftover startup
///                 residue that was on main before altscreen entry.
///
/// Window title / icon and progress state are restored separately by
/// `terminal_tab_state_restore_bytes`, written right after this.
fn terminal_normalize_bytes() -> &'static [u8] {
    b"\x1b[?1049l\x1b[?2026l\x1b[!p\x1b[0m\x1b[?25h\x1b[0 q\
        \x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?2004l\
        \x1b[H\x1b[2J"
}

pub struct RawModeGuard {
    cleaned_up: bool,
}

impl RawModeGuard {
    pub fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let _ = std::io::stdout().write_all(terminal_tab_state_save_bytes());
        // Keep local bracketed paste enabled so terminal pastes arrive as a
        // single Event::Paste even when the child app itself has not enabled
        // DECSET 2004. Whether we wrap forwarded bytes for the child is a
        // separate decision handled in the attach client.
        let _ = execute!(std::io::stdout(), EnableBracketedPaste);
        Ok(Self { cleaned_up: false })
    }

    pub fn teardown_terminal(&mut self) -> Result<()> {
        if self.cleaned_up {
            return Ok(());
        }

        let mut first_error: Option<AppError> = None;
        if let Err(err) = terminal::disable_raw_mode() {
            first_error = Some(err.into());
        }

        let mut stdout = std::io::stdout();

        if let Err(err) = stdout.write_all(terminal_normalize_bytes()) {
            if first_error.is_none() {
                first_error = Some(err.into());
            }
        }

        // crossterm also tracks its own bracketed-paste / mouse state.
        let execute_result = execute!(stdout, DisableBracketedPaste, DisableMouseCapture);
        if let Err(err) = execute_result {
            if first_error.is_none() {
                first_error = Some(err.into());
            }
        }

        if let Err(err) = stdout.write_all(terminal_tab_state_restore_bytes()) {
            if first_error.is_none() {
                first_error = Some(err.into());
            }
        }

        if let Err(err) = stdout.flush() {
            if first_error.is_none() {
                first_error = Some(err.into());
            }
        }

        self.cleaned_up = true;

        if let Some(err) = first_error {
            Err(err)
        } else {
            Ok(())
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = self.teardown_terminal();
    }
}

pub struct ColorfulGuard {
    enabled: bool,
}

impl ColorfulGuard {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }
}

impl Drop for ColorfulGuard {
    fn drop(&mut self) {
        if self.enabled {
            let _ = std::io::stdout().write_all(b"\x1b[0m\x1b[39m\x1b[49m\x1b[?25h");
            let _ = std::io::stdout().flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        terminal_normalize_bytes, terminal_tab_state_restore_bytes, terminal_tab_state_save_bytes,
    };

    #[test]
    fn terminal_tab_state_save_uses_title_stack_push() {
        assert_eq!(terminal_tab_state_save_bytes(), b"\x1b[22;0t");
    }

    #[test]
    fn terminal_tab_state_restore_restores_title_and_clears_progress() {
        assert_eq!(
            terminal_tab_state_restore_bytes(),
            b"\x1b[23;0t\x1b]9;4;0;0\x07"
        );
    }

    #[test]
    fn terminal_normalize_leaves_alt_screen_before_disabling_synchronized_output() {
        let bytes = terminal_normalize_bytes();
        let alt_screen_pos = find_subslice(bytes, b"\x1b[?1049l").expect("leaves alt screen");
        let sync_off_pos =
            find_subslice(bytes, b"\x1b[?2026l").expect("disables synchronized output");
        assert!(
            alt_screen_pos < sync_off_pos,
            "expected alt-screen exit before disabling synchronized output"
        );
    }

    #[test]
    fn terminal_normalize_disables_synchronized_output_so_crashes_cannot_freeze_the_terminal() {
        // The Windows attach renderer wraps repaints in \x1b[?2026h .. \x1b[?2026l.
        // If the client crashes or detaches between the two, teardown must
        // still emit \x1b[?2026l or the terminal is left buffering redraws
        // forever.
        assert!(find_subslice(terminal_normalize_bytes(), b"\x1b[?2026l").is_some());
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }
}
