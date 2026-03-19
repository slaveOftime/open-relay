// ---------------------------------------------------------------------------
// RawModeGuard - RAII for terminal raw mode
// ---------------------------------------------------------------------------

use std::io::Write;

use crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste},
    execute, terminal,
};

use crate::error::{AppError, Result};

pub struct RawModeGuard {
    cleaned_up: bool,
}

impl RawModeGuard {
    pub fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;
        // Enable bracketed paste so multi-line pastes arrive as a single
        // Event::Paste rather than being injected as individual key events
        // (which would fire Enter after each line).
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

        // Unconditional terminal normalisation.  The attached process may have
        // entered its own alternate screen, changed cursor-key mode, enabled
        // mouse tracking, etc.  We undo all of that:
        //
        //  \x1b[?1049l  - leave alternate screen (no-op if already on main).
        //                 For TUI children this restores the main screen;
        //                 for non-TUI children (REPLs, shells) this is a
        //                 no-op and their output stays in scrollback.
        //  \x1b[!p      - DECSTR soft terminal reset (resets DECCKM, DECOM,
        //                 DECAWM, scroll region, etc. without clearing screen)
        //  \x1b[0m      - SGR reset (colors / bold / etc.)
        //  \x1b[?25h    - ensure cursor is visible
        //  \x1b[0 q     - reset cursor style to terminal default (restores
        //                 blinking); DECSCUSR with param 0
        //  \x1b[?1000l .. \x1b[?2004l  - disable mouse and bracketed-paste
        //                 modes the app may have enabled (belt-and-suspenders
        //                 alongside crossterm's DisableBracketedPaste below)
        //  \x1b[H\x1b[2J - home cursor then erase entire display.  On
        //                 modern terminals (VTE, xterm, kitty, Windows
        //                 Terminal) ED 2 pushes the visible content into
        //                 scrollback, so session output remains accessible
        //                 via scroll-up.  This gives the post-detach status
        //                 message and shell prompt a clean screen.  For TUI
        //                 children, \x1b[?1049l already restored the main
        //                 screen, so this clears any leftover startup
        //                 residue that was on main before altscreen entry.
        let normalize: &[u8] = b"\x1b[?1049l\x1b[!p\x1b[0m\x1b[?25h\x1b[0 q\
            \x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?2004l\
            \x1b[H\x1b[2J";
        if let Err(err) = stdout.write_all(normalize) {
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
