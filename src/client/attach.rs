use std::io::Write;

use crossterm::{
    cursor,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers,
    },
    execute, terminal,
};
use tokio::{io::BufReader, sync::mpsc};

use crate::{
    config::AppConfig,
    error::{AppError, Result},
    ipc,
    protocol::{RpcRequest, RpcResponse},
};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub async fn run_attach(config: &AppConfig, id: &str) -> Result<()> {
    run_attach_inner(config, id, None).await
}

pub async fn run_attach_node(config: &AppConfig, id: &str, node: Option<String>) -> Result<()> {
    run_attach_inner(config, id, node.as_deref()).await
}

async fn run_attach_inner(config: &AppConfig, id: &str, node: Option<&str>) -> Result<()> {
    let stream = ipc::connect(config).await?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    // Send AttachSubscribe (wrapped in NodeProxy if targeting a remote node).
    ipc::write_request_to_writer(
        &mut write_half,
        attach_proxy(
            node,
            RpcRequest::AttachSubscribe {
                id: id.to_string(),
                from_byte_offset: None,
            },
        ),
    )
    .await?;

    // Receive the init frame.
    let init = ipc::read_response_from_reader(&mut reader).await?;
    let (initial_data, mut running, mut child_app_cursor_keys) = match init {
        RpcResponse::AttachStreamInit {
            data,
            running,
            app_cursor_keys,
            ..
        } => (data, running, app_cursor_keys),
        RpcResponse::Error { message } => return Err(AppError::DaemonUnavailable(message)),
        _ => return Err(AppError::Protocol("unexpected response type".to_string())),
    };

    let mut detached = false;
    let mut stream_error: Option<AppError> = None;
    {
        let mut raw_mode = RawModeGuard::new()?;
        // Use the terminal's alternate screen buffer so that TUI draw
        // commands (cursor positioning, color etc.) never touch the main
        // screen or scrollback.  The main screen and its history are fully
        // restored when we leave the alternate screen on teardown.
        raw_mode.enter_alternate_screen()?;

        // Initial resize + render.
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        ipc::write_request_to_writer(
            &mut write_half,
            RpcRequest::AttachResize {
                id: id.to_string(),
                rows,
                cols,
            },
        )
        .await?;

        // Write the initial replay bytes directly — the daemon has already
        // stripped CPR/DSR responses via EscapeFilter, so no further
        // processing is needed.  Writing raw bytes preserves all cursor-
        // positioning, color, and alternate-screen sequences that TUIs emit,
        // which is required for correct reattach rendering.
        {
            use std::io::Write;
            let mut stdout = std::io::stdout();
            stdout.write_all(&initial_data)?;
            stdout.flush()?;
        }

        // `read_response_from_reader` uses `read_line`, which is not safe to
        // keep cancelling with timeouts. Read daemon frames in a dedicated task
        // and receive them over a channel instead.
        let (frame_tx, mut frame_rx) = mpsc::unbounded_channel();
        let reader_task = tokio::spawn(async move {
            let mut reader = reader;
            loop {
                let frame = ipc::read_response_from_reader(&mut reader).await;
                let done = frame.is_err();
                if frame_tx.send(frame).is_err() {
                    break;
                }
                if done {
                    break;
                }
            }
        });

        while running {
            // Drain all pending keyboard/resize events first.
            loop {
                match event::poll(std::time::Duration::from_millis(0)) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(err) => {
                        stream_error = Some(err.into());
                        running = false;
                        break;
                    }
                }
                match event::read()? {
                    Event::Paste(data) => {
                        ipc::write_request_to_writer(
                            &mut write_half,
                            RpcRequest::AttachInput {
                                id: id.to_string(),
                                data,
                            },
                        )
                        .await?
                    }
                    Event::Resize(cols, rows) => {
                        ipc::write_request_to_writer(
                            &mut write_half,
                            RpcRequest::AttachResize {
                                id: id.to_string(),
                                rows,
                                cols,
                            },
                        )
                        .await?
                    }
                    Event::Key(key) => {
                        if !matches!(key.kind, KeyEventKind::Press) {
                            continue;
                        }
                        if is_ctrl_d(key) {
                            detached = true;
                            running = false;
                            break;
                        }
                        if let Some(data) = map_key_to_input(key, child_app_cursor_keys) {
                            ipc::write_request_to_writer(
                                &mut write_half,
                                RpcRequest::AttachInput {
                                    id: id.to_string(),
                                    data,
                                },
                            )
                            .await?;
                        }
                    }
                    _ => {}
                }
            }
            if !running {
                break;
            }

            // Wait for next server frame (with a timeout so we keep draining input).
            let frame =
                tokio::time::timeout(std::time::Duration::from_millis(60), frame_rx.recv()).await;
            match frame {
                Err(_timeout) => continue,
                Ok(None) => {
                    stream_error = Some(AppError::Protocol(
                        "daemon closed the connection".to_string(),
                    ));
                    break;
                }
                Ok(Some(Err(err))) => {
                    stream_error = Some(err);
                    break;
                }
                Ok(Some(Ok(RpcResponse::AttachStreamChunk { data, .. }))) => {
                    // Daemon already filtered CPR/DSR via EscapeFilter; write raw.
                    use std::io::Write;
                    std::io::stdout().write_all(&data)?;
                    std::io::stdout().flush()?;
                }
                Ok(Some(Ok(RpcResponse::AttachModeChanged {
                    app_cursor_keys, ..
                }))) => {
                    child_app_cursor_keys = app_cursor_keys;
                }
                Ok(Some(Ok(RpcResponse::AttachStreamDone { .. }))) => {
                    running = false;
                }
                Ok(Some(Ok(RpcResponse::Error { message }))) => {
                    return Err(AppError::DaemonUnavailable(message));
                }
                Ok(Some(Ok(_))) => {}
            }
        }

        if detached {
            // Detach while raw mode is still active, then consume any queued
            // key-release or terminal-response events so they do not leak into
            // the parent shell after we restore the terminal.
            let _ = ipc::write_request_to_writer(
                &mut write_half,
                RpcRequest::AttachDetach { id: id.to_string() },
            )
            .await;
            let _ = drain_pending_terminal_events();
        }

        reader_task.abort();
    }

    if detached {
        println!("Detached from session {id}");
    } else if let Some(err) = stream_error {
        eprintln!("Attach session {id} ended with error: {err}");
    } else {
        println!("Session {id} has ended.");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// RawModeGuard - RAII for terminal raw mode + optional alternate screen
// ---------------------------------------------------------------------------

pub struct RawModeGuard {
    cleaned_up: bool,
    alternate_screen: bool,
}

impl RawModeGuard {
    pub fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;
        // Enable bracketed paste so multi-line pastes arrive as a single
        // Event::Paste rather than being injected as individual key events
        // (which would fire Enter after each line).
        let _ = execute!(std::io::stdout(), EnableBracketedPaste);
        Ok(Self {
            cleaned_up: false,
            alternate_screen: false,
        })
    }

    #[allow(dead_code)]
    pub fn enter_alternate_screen(&mut self) -> Result<()> {
        if self.alternate_screen {
            return Ok(());
        }
        let mut stdout = std::io::stdout();
        execute!(
            stdout,
            terminal::EnterAlternateScreen,
            cursor::MoveTo(0, 0),
            terminal::Clear(terminal::ClearType::All)
        )?;
        stdout.flush()?;
        self.alternate_screen = true;
        Ok(())
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
        //  \x1b[?1049l  - leave alternate screen (no-op if already on main)
        //  \x1b[!p      - DECSTR soft terminal reset (resets DECCKM, DECOM,
        //                 DECAWM, scroll region, etc. without clearing screen)
        //  \x1b[0m      - SGR reset (colors / bold / etc.)
        //  \x1b[?25h    - ensure cursor is visible
        //  \x1b[?1000l .. \x1b[?2004l  - disable mouse and bracketed-paste
        //                 modes the app may have enabled (belt-and-suspenders
        //                 alongside crossterm's DisableBracketedPaste below)
        //
        // We deliberately do NOT reposition the cursor: after leaving altscreen
        // the cursor is where the main-screen left it (correct), and if we
        // were already on main screen the shell will handle its own prompt.
        let normalize: &[u8] = b"\x1b[?1049l\x1b[!p\x1b[0m\x1b[?25h\
            \x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?2004l";
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

// ---------------------------------------------------------------------------
// Terminal output helpers
// ---------------------------------------------------------------------------

fn attach_proxy(node: Option<&str>, req: RpcRequest) -> RpcRequest {
    match node {
        None => req,
        Some(name) => RpcRequest::NodeProxy {
            node: name.to_string(),
            inner: Box::new(req),
        },
    }
}

fn drain_pending_terminal_events() -> Result<()> {
    while event::poll(std::time::Duration::from_millis(0))? {
        let _ = event::read()?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Key input mapping
// ---------------------------------------------------------------------------

fn map_key_to_input(key: KeyEvent, app_cursor_keys: bool) -> Option<String> {
    match key.code {
        KeyCode::Enter => Some("\r".to_string()),
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                Some("\x1b[Z".to_string())
            } else {
                Some("\t".to_string())
            }
        }
        KeyCode::BackTab => Some("\x1b[Z".to_string()),
        KeyCode::Backspace => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                // Ctrl+Backspace → ASCII BS (0x08) for better compatibility with apps that do not handle DEL.
                Some("\x08".to_string())
            } else {
                // Backspace → ASCII DEL (0x7f) by default, which is what most terminals send and what most apps expect for the Backspace key.
                Some("\x7f".to_string())
            }
        }
        KeyCode::Esc => Some("\x1b".to_string()),
        KeyCode::Up => Some(if app_cursor_keys { "\x1bOA" } else { "\x1b[A" }.to_string()),
        KeyCode::Down => Some(if app_cursor_keys { "\x1bOB" } else { "\x1b[B" }.to_string()),
        KeyCode::Right => Some(if app_cursor_keys { "\x1bOC" } else { "\x1b[C" }.to_string()),
        KeyCode::Left => Some(if app_cursor_keys { "\x1bOD" } else { "\x1b[D" }.to_string()),
        KeyCode::Home => Some("\x1b[H".to_string()),
        KeyCode::End => Some("\x1b[F".to_string()),
        KeyCode::Delete => Some("\x1b[3~".to_string()),
        KeyCode::Insert => Some("\x1b[2~".to_string()),
        KeyCode::PageUp => Some("\x1b[5~".to_string()),
        KeyCode::PageDown => Some("\x1b[6~".to_string()),
        KeyCode::Char(c) => {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                let code = (c.to_ascii_lowercase() as u8) & 0x1f;
                Some((code as char).to_string())
            } else {
                Some(c.to_string())
            }
        }
        _ => None,
    }
}

fn is_ctrl_d(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    // -----------------------------------------------------------------------
    // Helper constructors
    // -----------------------------------------------------------------------

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn ctrl_press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn shift_press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    // -----------------------------------------------------------------------
    // map_key_to_input – basic keys
    // -----------------------------------------------------------------------

    #[test]
    fn test_map_key_enter() {
        assert_eq!(
            map_key_to_input(press(KeyCode::Enter), false),
            Some("\r".to_string())
        );
    }

    #[test]
    fn test_map_key_tab() {
        assert_eq!(
            map_key_to_input(press(KeyCode::Tab), false),
            Some("\t".to_string())
        );
    }

    #[test]
    fn test_map_key_backspace() {
        assert_eq!(
            map_key_to_input(press(KeyCode::Backspace), false),
            Some("\x7f".to_string())
        );
    }

    #[test]
    fn test_map_key_esc() {
        assert_eq!(
            map_key_to_input(press(KeyCode::Esc), false),
            Some("\x1b".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // map_key_to_input – shift+tab produces backtab sequence
    // -----------------------------------------------------------------------

    #[test]
    fn test_map_key_backtab_produces_shift_tab_sequence() {
        // crossterm fires BackTab for Shift-Tab regardless of platform.
        assert_eq!(
            map_key_to_input(press(KeyCode::BackTab), false),
            Some("\x1b[Z".to_string())
        );
    }

    #[test]
    fn test_map_key_tab_with_shift_modifier_produces_backtab_sequence() {
        assert_eq!(
            map_key_to_input(shift_press(KeyCode::Tab), false),
            Some("\x1b[Z".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // map_key_to_input – arrow keys (normal cursor mode)
    // -----------------------------------------------------------------------

    #[test]
    fn test_map_key_arrows_normal_mode() {
        assert_eq!(
            map_key_to_input(press(KeyCode::Up), false),
            Some("\x1b[A".to_string())
        );
        assert_eq!(
            map_key_to_input(press(KeyCode::Down), false),
            Some("\x1b[B".to_string())
        );
        assert_eq!(
            map_key_to_input(press(KeyCode::Right), false),
            Some("\x1b[C".to_string())
        );
        assert_eq!(
            map_key_to_input(press(KeyCode::Left), false),
            Some("\x1b[D".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // map_key_to_input – arrow keys (application cursor key mode / DECCKM)
    // -----------------------------------------------------------------------

    #[test]
    fn test_map_key_arrows_app_cursor_mode_uses_o_prefix() {
        assert_eq!(
            map_key_to_input(press(KeyCode::Up), true),
            Some("\x1bOA".to_string())
        );
        assert_eq!(
            map_key_to_input(press(KeyCode::Down), true),
            Some("\x1bOB".to_string())
        );
        assert_eq!(
            map_key_to_input(press(KeyCode::Right), true),
            Some("\x1bOC".to_string())
        );
        assert_eq!(
            map_key_to_input(press(KeyCode::Left), true),
            Some("\x1bOD".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // map_key_to_input – navigation / editing keys
    // -----------------------------------------------------------------------

    #[test]
    fn test_map_key_home_end() {
        assert_eq!(
            map_key_to_input(press(KeyCode::Home), false),
            Some("\x1b[H".to_string())
        );
        assert_eq!(
            map_key_to_input(press(KeyCode::End), false),
            Some("\x1b[F".to_string())
        );
    }

    #[test]
    fn test_map_key_delete_insert() {
        assert_eq!(
            map_key_to_input(press(KeyCode::Delete), false),
            Some("\x1b[3~".to_string())
        );
        assert_eq!(
            map_key_to_input(press(KeyCode::Insert), false),
            Some("\x1b[2~".to_string())
        );
    }

    #[test]
    fn test_map_key_page_up_down() {
        assert_eq!(
            map_key_to_input(press(KeyCode::PageUp), false),
            Some("\x1b[5~".to_string())
        );
        assert_eq!(
            map_key_to_input(press(KeyCode::PageDown), false),
            Some("\x1b[6~".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // map_key_to_input – printable characters and ctrl combos
    // -----------------------------------------------------------------------

    #[test]
    fn test_map_key_regular_chars_pass_through() {
        assert_eq!(
            map_key_to_input(press(KeyCode::Char('a')), false),
            Some("a".to_string())
        );
        assert_eq!(
            map_key_to_input(press(KeyCode::Char('Z')), false),
            Some("Z".to_string())
        );
        assert_eq!(
            map_key_to_input(press(KeyCode::Char('5')), false),
            Some("5".to_string())
        );
    }

    #[test]
    fn test_map_key_ctrl_c_produces_etx() {
        // Ctrl-C → ASCII 3 (ETX / SIGINT).
        let result = map_key_to_input(ctrl_press(KeyCode::Char('c')), false).unwrap();
        assert_eq!(result.as_bytes(), &[3]);
    }

    #[test]
    fn test_map_key_ctrl_d_produces_eot() {
        // Ctrl-D → ASCII 4 (EOT / EOF).
        let result = map_key_to_input(ctrl_press(KeyCode::Char('d')), false).unwrap();
        assert_eq!(result.as_bytes(), &[4]);
    }

    #[test]
    fn test_map_key_ctrl_z_produces_sub() {
        // Ctrl-Z → ASCII 26 (SUB / suspend).
        let result = map_key_to_input(ctrl_press(KeyCode::Char('z')), false).unwrap();
        assert_eq!(result.as_bytes(), &[26]);
    }

    // -----------------------------------------------------------------------
    // is_ctrl_d
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_ctrl_d_true() {
        assert!(is_ctrl_d(ctrl_press(KeyCode::Char('d'))));
        assert!(is_ctrl_d(ctrl_press(KeyCode::Char('D'))));
    }

    #[test]
    fn test_is_ctrl_d_false_for_plain_d() {
        assert!(!is_ctrl_d(press(KeyCode::Char('d'))));
    }

    #[test]
    fn test_is_ctrl_d_false_for_other_ctrl() {
        assert!(!is_ctrl_d(ctrl_press(KeyCode::Char('c'))));
    }
}
