use super::constants::{
    LIST_WINDOW_TITLE, TITLE_RESTORE_BYTES, TITLE_SAVE_BYTES, TUI_RESTORE_BYTES,
};
use crate::error::Result;
use crossterm::{
    cursor::{Hide, Show},
    event::{Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Frame, Terminal, backend::CrosstermBackend};
use std::io::{self, Write};

pub fn wait_for_ctrl_d() -> Result<()> {
    println!("\nPress Ctrl+D to return to the session list");
    io::stdout().flush()?;
    enable_raw_mode()?;
    let result = loop {
        match crossterm::event::read() {
            Ok(Event::Key(key))
                if key.kind != KeyEventKind::Release
                    && key.code == KeyCode::Char('d')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                break Ok(());
            }
            Ok(_) => {}
            Err(error) => break Err(error.into()),
        }
    };
    let _ = disable_raw_mode();
    result
}

pub struct TuiTerminal {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    cleaned_up: bool,
    title_saved: bool,
}

impl TuiTerminal {
    pub(super) fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            let _ = restore_tui_state(&mut stdout);
            #[cfg(windows)]
            crate::client::crash::set_tui_active(false);
            return Err(error.into());
        }
        // Claim the window title: push the current one onto the terminal's
        // title stack, then set ours. `title_saved` tracks whether the push
        // happened so teardown (and the error paths below) only pop when there
        // is a matching entry to restore.
        let title_saved = enter_list_title(&mut stdout).is_ok();
        #[cfg(windows)]
        crate::client::crash::set_tui_active(true);
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self {
                terminal,
                cleaned_up: false,
                title_saved,
            }),
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = disable_raw_mode();
                if title_saved {
                    let _ = stdout.write_all(TITLE_RESTORE_BYTES);
                }
                let _ = restore_tui_state(&mut stdout);
                #[cfg(windows)]
                crate::client::crash::set_tui_active(false);
                Err(error.into())
            }
        }
    }

    pub(super) fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> Result<()> {
        self.terminal.draw(render)?;
        Ok(())
    }

    /// Hand the terminal to a child process (`oly attach` / `oly logs`) on the
    /// main screen.  Leaving the alternate buffer lets the child render like a
    /// natively running CLI: its output flows into the terminal's scrollback,
    /// so history and the scrollbar keep working during and after the inline
    /// view.  Only raw mode is released, because the child installs its own.
    pub(super) fn suspend(&mut self) -> Result<()> {
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show)?;
        Ok(())
    }

    /// Take the terminal back after the child exits.  `EnterAlternateScreen` is
    /// re-issued unconditionally: an attached child's teardown emits
    /// `\x1b[?1049l`, which drops the terminal back to the main buffer even
    /// though we never left it ourselves.
    pub(super) fn resume(&mut self) -> Result<()> {
        enable_raw_mode()?;
        execute!(self.terminal.backend_mut(), EnterAlternateScreen, Hide)?;
        // Re-assert our window title. The inline child (oly attach / oly logs)
        // may have forwarded its own OSC title, and its teardown only restores
        // the previous one on terminals that support the XTWINOPS title stack.
        let _ = write_list_title(self.terminal.backend_mut());
        self.terminal.clear()?;
        Ok(())
    }

    pub(super) fn teardown(&mut self) -> Result<()> {
        if self.cleaned_up {
            return Ok(());
        }

        let mut first_error = disable_raw_mode().err();
        if self.title_saved {
            if let Err(error) = self.terminal.backend_mut().write_all(TITLE_RESTORE_BYTES)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
            self.title_saved = false;
        }
        if let Err(error) = restore_tui_state(self.terminal.backend_mut())
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        #[cfg(windows)]
        crate::client::crash::set_tui_active(false);
        self.cleaned_up = true;

        if let Some(error) = first_error {
            Err(error.into())
        } else {
            Ok(())
        }
    }
}

impl Drop for TuiTerminal {
    fn drop(&mut self) {
        let _ = self.teardown();
    }
}

pub fn restore_tui_state(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(TUI_RESTORE_BYTES)?;
    writer.flush()
}

/// Push the current title onto the terminal's title stack and set the window
/// title for the session list. Returns `Ok` only when the whole sequence was
/// written, so callers know a matching restore is required.
pub fn enter_list_title(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(TITLE_SAVE_BYTES)?;
    write_list_title(writer)?;
    writer.flush()
}

/// OSC 0 sets both the icon and window title while the session list is on
/// screen.
pub fn write_list_title(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(b"\x1b]0;")?;
    writer.write_all(LIST_WINDOW_TITLE.as_bytes())?;
    writer.write_all(b"\x07")
}
