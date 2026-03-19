use std::io::{IsTerminal, Write};

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute, terminal,
};
use tokio::{io::BufReader, sync::mpsc};

use crate::{
    clipboard,
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

    // Pre-allocate once — avoids 7+ repeated heap allocations of the same id.
    let id_owned = id.to_string();

    // Send AttachSubscribe (wrapped in NodeProxy if targeting a remote node).
    ipc::write_request_to_writer(
        &mut write_half,
        attach_proxy(
            node,
            RpcRequest::AttachSubscribe {
                id: id_owned.clone(),
                from_byte_offset: None,
            },
        ),
    )
    .await?;

    // Receive the init frame.
    let init = ipc::read_response_from_reader(&mut reader).await?;
    let (initial_data, mut running, mut child_bracketed_paste_mode, mut child_app_cursor_keys) =
        match init {
            RpcResponse::AttachStreamInit {
                data,
                running,
                bracketed_paste_mode,
                app_cursor_keys,
                ..
            } => (data, running, bracketed_paste_mode, app_cursor_keys),
            RpcResponse::Error { message } => return Err(AppError::DaemonUnavailable(message)),
            _ => return Err(AppError::Protocol("unexpected response type".to_string())),
        };

    // When stdio is piped, interactive terminal control fails across platforms,
    // so fall back to a plain stream replay instead of raw-mode attach.
    if !can_use_interactive_terminal() {
        write_bytes_to_stdout(&initial_data)?;
        drop(initial_data); // Release up to 1 MB of replay data immediately.

        while running {
            match ipc::read_response_from_reader(&mut reader).await? {
                RpcResponse::AttachStreamChunk { data, .. } => write_bytes_to_stdout(&data)?,
                RpcResponse::AttachModeChanged { .. } => {}
                RpcResponse::AttachStreamDone { .. } => {
                    running = false;
                }
                RpcResponse::Error { message } => {
                    return Err(AppError::DaemonUnavailable(message));
                }
                _ => {}
            }
        }

        println!("Session {id} has ended.");
        return Ok(());
    }

    let mut detached = false;
    let mut stream_error: Option<AppError> = None;
    {
        let _raw_mode = RawModeGuard::new()?;

        // Initial resize + render.
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let mut last_sent_size = (cols, rows);
        ipc::write_request_to_writer(
            &mut write_half,
            RpcRequest::AttachResize {
                id: id_owned.clone(),
                rows,
                cols,
            },
        )
        .await?;

        // Clear the visible screen and home the cursor before writing
        // replay data.  The replay contains filtered PTY output whose cursor
        // positioning (absolute and relative) was calculated from row 1,
        // col 1.  Without this clear, the replay starts from wherever the
        // terminal cursor happens to be, offsetting all subsequent cursor
        // operations and causing line-editing (backspace, arrow keys) in
        // REPLs to target the wrong screen position.
        //
        // Most modern terminals push the visible content into scrollback
        // on ED 2, so previous history remains scrollable.
        write_bytes_to_stdout(b"\x1b[H\x1b[2J")?;

        // Write the initial replay bytes directly — the daemon has already
        // stripped CPR/DSR responses via EscapeFilter, so no further
        // processing is needed.  Writing raw bytes preserves all cursor-
        // positioning, color, and alternate-screen sequences that TUIs emit,
        // which is required for correct reattach rendering.
        write_bytes_to_stdout(&initial_data)?;
        drop(initial_data); // Release up to 1 MB of replay data immediately.

        // Drain any stale resize events queued by writing replay data
        // before the main event loop.
        let _ = drain_pending_terminal_events();

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
                                id: id_owned.clone(),
                                data: wrap_paste_input(data, child_bracketed_paste_mode),
                            },
                        )
                        .await?
                    }
                    Event::Resize(cols, rows) => {
                        // Re-read the actual terminal size — the event may
                        // carry stale dimensions on some platforms.
                        let (actual_cols, actual_rows) = terminal::size().unwrap_or((cols, rows));
                        if (actual_cols, actual_rows) != last_sent_size {
                            last_sent_size = (actual_cols, actual_rows);
                            ipc::write_request_to_writer(
                                &mut write_half,
                                RpcRequest::AttachResize {
                                    id: id_owned.clone(),
                                    rows: actual_rows,
                                    cols: actual_cols,
                                },
                            )
                            .await?
                        }
                    }
                    Event::Key(key) => {
                        if is_ctrl_d(key) {
                            detached = true;
                            running = false;
                            break;
                        }

                        if let Some(data) = maybe_collect_clipboard_paste(
                            config,
                            id,
                            node,
                            key,
                            child_bracketed_paste_mode,
                        )? {
                            ipc::write_request_to_writer(
                                &mut write_half,
                                RpcRequest::AttachInput {
                                    id: id_owned.clone(),
                                    data,
                                },
                            )
                            .await?;
                            continue;
                        }

                        if !matches!(key.kind, KeyEventKind::Press) {
                            continue;
                        }

                        if let Some(data) = map_key_to_input(key, child_app_cursor_keys) {
                            ipc::write_request_to_writer(
                                &mut write_half,
                                RpcRequest::AttachInput {
                                    id: id_owned.clone(),
                                    data,
                                },
                            )
                            .await?;
                        }
                    }
                    Event::Mouse(mouse) => {
                        let data = map_mouse_to_sgr_input(mouse);
                        ipc::write_request_to_writer(
                            &mut write_half,
                            RpcRequest::AttachInput {
                                id: id_owned.clone(),
                                data,
                            },
                        )
                        .await?;
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
                    write_bytes_to_stdout(&data)?;
                }
                Ok(Some(Ok(RpcResponse::AttachModeChanged {
                    app_cursor_keys,
                    bracketed_paste_mode,
                }))) => {
                    child_app_cursor_keys = app_cursor_keys;
                    child_bracketed_paste_mode = bracketed_paste_mode;
                }
                Ok(Some(Ok(RpcResponse::AttachResized { rows: _, cols: _ }))) => {
                    // Another client resized the PTY.  We cannot
                    // programmatically resize the terminal window (only the
                    // screen buffer on Windows, which corrupts the display).
                    // Instead, update last_sent_size to the actual terminal
                    // size so that the dedup guard in Event::Resize prevents
                    // echoing our unchanged dimensions back to the server.
                    let (actual_cols, actual_rows) = terminal::size().unwrap_or((80, 24));
                    last_sent_size = (actual_cols, actual_rows);
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
                RpcRequest::AttachDetach {
                    id: id_owned.clone(),
                },
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
// RawModeGuard - RAII for terminal raw mode
// ---------------------------------------------------------------------------

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

fn can_use_interactive_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn write_bytes_to_stdout(data: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout();
    stdout.write_all(data)?;
    stdout.flush()?;
    Ok(())
}

fn wrap_paste_input(data: String, bracketed_paste_mode: bool) -> String {
    if bracketed_paste_mode {
        format!("\x1b[200~{data}\x1b[201~")
    } else {
        data
    }
}

fn maybe_collect_clipboard_paste(
    config: &AppConfig,
    id: &str,
    node: Option<&str>,
    key: KeyEvent,
    bracketed_paste_mode: bool,
) -> Result<Option<String>> {
    if node.is_some() || !is_clipboard_paste_key(key) {
        return Ok(None);
    }

    Ok(clipboard::collect_clipboard_paste(config, id)?
        .map(|data| wrap_paste_input(data, bracketed_paste_mode)))
}

fn is_clipboard_paste_key(key: KeyEvent) -> bool {
    (key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V')))
        || (key.modifiers.contains(KeyModifiers::SHIFT) && matches!(key.code, KeyCode::Insert))
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

fn map_mouse_to_sgr_input(mouse: MouseEvent) -> String {
    let mut cb: u16 = match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => 0,
        MouseEventKind::Down(MouseButton::Middle) => 1,
        MouseEventKind::Down(MouseButton::Right) => 2,
        MouseEventKind::Up(MouseButton::Left) => 0,
        MouseEventKind::Up(MouseButton::Middle) => 1,
        MouseEventKind::Up(MouseButton::Right) => 2,
        MouseEventKind::Drag(MouseButton::Left) => 32,
        MouseEventKind::Drag(MouseButton::Middle) => 33,
        MouseEventKind::Drag(MouseButton::Right) => 34,
        MouseEventKind::Moved => 35,
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        MouseEventKind::ScrollLeft => 66,
        MouseEventKind::ScrollRight => 67,
    };
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        cb += 4;
    }
    if mouse.modifiers.contains(KeyModifiers::ALT) {
        cb += 8;
    }
    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
        cb += 16;
    }
    // SGR uses 1-based coordinates.
    let cx = mouse.column + 1;
    let cy = mouse.row + 1;
    let suffix = if matches!(mouse.kind, MouseEventKind::Up(_)) {
        'm'
    } else {
        'M'
    };
    format!("\x1b[<{cb};{cx};{cy}{suffix}")
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

    #[test]
    fn test_wrap_paste_input_passthrough_when_bracketed_paste_is_disabled() {
        assert_eq!(
            wrap_paste_input("hello\nworld".to_string(), false),
            "hello\nworld"
        );
    }

    #[test]
    fn test_wrap_paste_input_wraps_when_bracketed_paste_is_enabled() {
        assert_eq!(
            wrap_paste_input("hello\nworld".to_string(), true),
            "\x1b[200~hello\nworld\x1b[201~"
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

    // -----------------------------------------------------------------------
    // map_mouse_to_sgr_input
    // -----------------------------------------------------------------------

    fn mouse_event(kind: MouseEventKind, col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    fn mouse_event_with_mods(
        kind: MouseEventKind,
        col: u16,
        row: u16,
        modifiers: KeyModifiers,
    ) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers,
        }
    }

    #[test]
    fn test_mouse_left_press() {
        let ev = mouse_event(MouseEventKind::Down(MouseButton::Left), 9, 4);
        assert_eq!(map_mouse_to_sgr_input(ev), "\x1b[<0;10;5M");
    }

    #[test]
    fn test_mouse_right_release() {
        let ev = mouse_event(MouseEventKind::Up(MouseButton::Right), 0, 0);
        assert_eq!(map_mouse_to_sgr_input(ev), "\x1b[<2;1;1m");
    }

    #[test]
    fn test_mouse_middle_drag() {
        let ev = mouse_event(MouseEventKind::Drag(MouseButton::Middle), 5, 10);
        assert_eq!(map_mouse_to_sgr_input(ev), "\x1b[<33;6;11M");
    }

    #[test]
    fn test_mouse_scroll_up() {
        let ev = mouse_event(MouseEventKind::ScrollUp, 20, 15);
        assert_eq!(map_mouse_to_sgr_input(ev), "\x1b[<64;21;16M");
    }

    #[test]
    fn test_mouse_scroll_down() {
        let ev = mouse_event(MouseEventKind::ScrollDown, 20, 15);
        assert_eq!(map_mouse_to_sgr_input(ev), "\x1b[<65;21;16M");
    }

    #[test]
    fn test_mouse_moved() {
        let ev = mouse_event(MouseEventKind::Moved, 3, 7);
        assert_eq!(map_mouse_to_sgr_input(ev), "\x1b[<35;4;8M");
    }

    #[test]
    fn test_mouse_with_shift_modifier() {
        let ev = mouse_event_with_mods(
            MouseEventKind::Down(MouseButton::Left),
            0,
            0,
            KeyModifiers::SHIFT,
        );
        assert_eq!(map_mouse_to_sgr_input(ev), "\x1b[<4;1;1M");
    }

    #[test]
    fn test_mouse_with_ctrl_alt_modifiers() {
        let ev = mouse_event_with_mods(
            MouseEventKind::Down(MouseButton::Left),
            0,
            0,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert_eq!(map_mouse_to_sgr_input(ev), "\x1b[<24;1;1M");
    }
}
