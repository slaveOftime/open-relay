use std::io::{IsTerminal, Write};

use crossterm::{
    event::{
        self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    terminal,
};
use tokio::{io::BufReader, sync::mpsc};

use crate::{
    clipboard,
    config::AppConfig,
    error::{AppError, Result},
    ipc,
    protocol::{RpcRequest, RpcResponse},
};

use super::cursor::StreamCursor;

/// Upper bound on how many bytes of already-queued server output are written
/// to the terminal in one go. Batching turns a burst of frames into a single
/// blocking write plus flush; the cap keeps the terminal painting
/// incrementally rather than freezing on one very large write.
const MAX_BATCHED_FRAME_BYTES: usize = 1024 * 1024;

#[cfg(windows)]
struct AttachRenderer {
    parser: vt100::Parser,
    needs_full_repaint: bool,
}

#[cfg(windows)]
impl AttachRenderer {
    fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows.max(1), cols.max(1), 0),
            needs_full_repaint: false,
        }
    }

    fn render_initial(&mut self, data: &[u8]) -> Vec<u8> {
        self.parser.process(data);
        self.needs_full_repaint = false;
        let mut rendered = passthrough_signals(data);
        rendered.extend_from_slice(&self.parser.screen().state_formatted());
        rendered
    }

    fn render_chunk(&mut self, data: &[u8]) -> Vec<u8> {
        let previous = self.parser.screen().clone();
        self.parser.process(data);

        // The canonical screen state only models the grid, cursor and modes,
        // so window title and progress/busy notifications are forwarded from
        // the original bytes; otherwise they would be dropped entirely.
        let mut rendered = passthrough_signals(data);

        // Render from canonical screen state instead of forwarding ConPTY's
        // wrap-dependent bytes. Once the initial snapshot is on screen, state
        // diffs preserve that exact baseline without full-screen flashing.
        let update = if self.needs_full_repaint {
            self.needs_full_repaint = false;
            self.parser.screen().state_formatted()
        } else {
            self.parser.screen().state_diff(&previous)
        };
        if update.is_empty() {
            return rendered;
        }
        rendered.extend_from_slice(b"\x1b[?2026h");
        rendered.extend_from_slice(&update);
        rendered.extend_from_slice(b"\x1b[?2026l");
        rendered
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        // The attach renderer repaints only the visible screen, so it keeps
        // no scrollback of its own.
        crate::session::screen::safe_resize_parser(&mut self.parser, rows, cols, 0);
        self.needs_full_repaint = true;
    }
}

/// Extract the semantic terminal signals (window title, progress and busy
/// indicators, cursor shape) that the canonical screen state does not model,
/// so they survive a repaint driven by that state.
#[cfg(windows)]
fn passthrough_signals(data: &[u8]) -> Vec<u8> {
    let mut signals = crate::session::scan::extract_passthrough_osc_sequences(data);
    if let Some(params) = crate::session::scan::last_cursor_style_params(data) {
        signals.extend_from_slice(b"\x1b[");
        signals.extend_from_slice(params);
        signals.extend_from_slice(b" q");
    }
    signals
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub async fn run_attach(config: &AppConfig, id: &str, role: Option<&str>) -> Result<()> {
    run_attach_inner(config, id, None, role).await
}

pub async fn run_attach_node(
    config: &AppConfig,
    id: &str,
    node: Option<String>,
    role: Option<&str>,
) -> Result<()> {
    run_attach_inner(config, id, node.as_deref(), role).await
}

async fn run_attach_inner(
    config: &AppConfig,
    id: &str,
    node: Option<&str>,
    role: Option<&str>,
) -> Result<()> {
    let stream = ipc::connect(config).await?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let interactive = can_use_interactive_terminal();
    let initial_size = if interactive {
        terminal::size().ok()
    } else {
        None
    };

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
                incarnation: None,
                rows: initial_size.map(|(_, rows)| rows),
                cols: initial_size.map(|(cols, _)| cols),
                role: role.map(str::to_owned),
            },
        ),
    )
    .await?;

    // Receive the init frame.
    let init = ipc::read_checked_response_from_reader(&mut reader).await?;
    let (
        initial_data,
        scrollback_seed,
        mut running,
        mut child_bracketed_paste_mode,
        mut child_app_cursor_keys,
        stream_end_offset,
        granted_role,
    ) = match init {
        RpcResponse::AttachStreamInit {
            data,
            scrollback,
            running,
            bracketed_paste_mode,
            app_cursor_keys,
            end_offset,
            role,
            ..
        } => (
            data,
            scrollback,
            running,
            bracketed_paste_mode,
            app_cursor_keys,
            end_offset,
            role,
        ),
        _ => return Err(AppError::Protocol("unexpected response type".to_string())),
    };

    // I6 (PLAN §8.1): observers never drive input or geometry. The server
    // enforces the lease; this mirrors it client-side so an observer's
    // keystrokes don't bounce off the gate.
    let mut is_controller = granted_role != "observer";
    if interactive && !is_controller {
        eprintln!("Attached as observer (view-only). Ctrl-T takes control, Ctrl-D detaches.");
    }

    // Every chunk must continue exactly at the cursor the init frame left
    // us at; gaps/duplicates abort the attach loudly (I2, M3-3).
    let mut stream_cursor = StreamCursor::new(stream_end_offset);
    let mut last_acked: u64 = 0;

    // When stdio is piped, interactive terminal control fails across platforms,
    // so fall back to a plain stream replay instead of raw-mode attach.
    if !interactive {
        write_bytes_to_stdout(&initial_data)?;
        drop(initial_data); // Release up to 1 MB of replay data immediately.

        while running {
            match ipc::read_checked_response_from_reader(&mut reader).await? {
                RpcResponse::AttachStreamChunk { offset, data } => {
                    stream_cursor.accept(offset, data.len())?;
                    write_bytes_to_stdout(&data)?;
                    maybe_send_ack(
                        &mut write_half,
                        &id_owned,
                        stream_cursor.current(),
                        &mut last_acked,
                    )
                    .await;
                }
                RpcResponse::AttachModeChanged { .. } => {}
                RpcResponse::AttachControlChanged { .. } => {}
                RpcResponse::AttachStreamDone { final_offset, .. } => {
                    stream_cursor.finish(final_offset)?;
                    running = false;
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
        let _raw_mode = crate::terminal_guards::RawModeGuard::new()?;

        // Render the current terminal snapshot. The subscribe handshake already
        // asked the daemon to resize to the current terminal size when needed.
        let (cols, rows) = initial_size.unwrap_or_else(|| terminal::size().unwrap_or((80, 24)));
        let mut last_sent_size = (cols, rows);

        if scrollback_seed.is_empty() {
            // Clear the visible screen and home the cursor before writing
            // snapshot data so the restored screen starts from a known state.
            write_bytes_to_stdout(b"\x1b[H\x1b[2J")?;
        } else {
            // Print the session's recent history so the terminal scrollbar
            // reaches back past the attach point, then push it into
            // scrollback; the snapshot repaints the now-blank screen.
            write_bytes_to_stdout(&scrollback_seed_bytes(&scrollback_seed, rows))?;
        }

        #[cfg(windows)]
        let mut renderer = AttachRenderer::new(rows, cols);
        #[cfg(windows)]
        write_bytes_to_stdout(&renderer.render_initial(&initial_data))?;

        #[cfg(not(windows))]
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
                let frame = ipc::read_checked_response_from_reader(&mut reader).await;
                let done = frame.is_err();
                if frame_tx.send(frame).is_err() {
                    break;
                }
                if done {
                    break;
                }
            }
        });

        let mut shutdown_rx = spawn_attach_shutdown_listener();

        // Terminal events are produced by a dedicated blocking thread
        // (crossterm's `event::read` blocks); the async loop selects over
        // terminal events, daemon frames, and the shutdown signal. Input is
        // fully event-driven: no polling timeouts, no key-burst deadlines,
        // no paste-detection windows (PLAN.md §5.1/§5.2 — paste boundaries
        // come from bracketed-paste markers or the explicit clipboard
        // shortcut, never from typing speed).
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let event_reader = std::thread::spawn(move || {
            // stdin closed or unreadable ends the stream.
            while let Ok(ev) = event::read() {
                if event_tx.send(ev).is_err() {
                    break;
                }
            }
        });

        while running {
            tokio::select! {
                    biased;
                    _ = shutdown_rx.recv() => {
                        detached = true;
                        break;
                    }
                    maybe_event = event_rx.recv() => {
                        let Some(ev) = maybe_event else {
                            stream_error = Some(AppError::Protocol(
                                "terminal input stream ended".to_string(),
                            ));
                            break;
                        };
                        match ev {
                            Event::Paste(data) => {
                                // Explicit bracketed-paste boundaries: send the
                                // paste as one bounded transaction.
                                ipc::write_request_to_writer(
                                    &mut write_half,
                                    RpcRequest::AttachInput {
                                        id: id_owned.clone(),
                                        data: wrap_paste_input(
                                            normalize_paste_text(data),
                                            child_bracketed_paste_mode,
                                        ),
                                        wait_for_change: false,
                                    },
                                )
                                .await?
                            }
                            Event::Resize(cols, rows) => {
                                // Re-read the actual terminal size — the event may
                                // carry stale dimensions on some platforms.
                                let (actual_cols, actual_rows) =
                                    terminal::size().unwrap_or((cols, rows));
                                if !is_controller {
                                    // Observer: the resize is a viewport
                                    // preference, not session geometry.
                                    last_sent_size = (actual_cols, actual_rows);
                                } else if (actual_cols, actual_rows) != last_sent_size {
                                    last_sent_size = (actual_cols, actual_rows);

                                    #[cfg(windows)]
                                    renderer.resize(actual_rows, actual_cols);

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
                                if is_clipboard_paste_key(key) {
                                    // Explicit paste shortcut: read the clipboard
                                    // and send the paste in one bounded
                                    // transaction. An empty or unavailable
                                    // clipboard swallows the shortcut rather than
                                    // leaking a stray ^V into the session.
                                    if let Some(data) = maybe_collect_clipboard_paste(
                                        config,
                                        id,
                                        node,
                                        key,
                                        child_bracketed_paste_mode,
                                    )
                                    .await?
                                    {
                                        ipc::write_request_to_writer(
                                            &mut write_half,
                                            RpcRequest::AttachInput {
                                                id: id_owned.clone(),
                                                data,
                                                wait_for_change: true,
                                            },
                                        )
                                        .await?;
                                    }
                                } else if !matches!(key.kind, KeyEventKind::Press) {
                                    // Key release/repeat events: not sent.
                                } else if is_ctrl_d(key) {
                                    detached = true;
                                    running = false;
                                } else if !is_controller && is_ctrl_t(key) {
                                    // Observer takeover: the server answers
                                    // with an AttachControlChanged frame.
                                    ipc::write_request_to_writer(
                                        &mut write_half,
                                        RpcRequest::AttachAcquireControl {
                                            id: id_owned.clone(),
                                        },
                                    )
                                    .await?;
                                } else if !is_controller {
                                    // Observer: keys do not reach the session.
                                } else if let Some(data) = map_key_to_input(key, child_app_cursor_keys)
                                {
                                    // Every ordinary key is sent the moment it
                                    // arrives — no burst buffering.
                                    ipc::write_request_to_writer(
                                        &mut write_half,
                                        RpcRequest::AttachInput {
                                            id: id_owned.clone(),
                                            data,
                                            wait_for_change: false,
                                        },
                                    )
                                    .await?;
                                }
                            }
                            Event::Mouse(mouse) => {
                                if !is_controller {
                                    continue;
                                }
                                let data = map_mouse_to_sgr_input(mouse);
                                ipc::write_request_to_writer(
                                    &mut write_half,
                                    RpcRequest::AttachInput {
                                        id: id_owned.clone(),
                                        data,
                                        wait_for_change: false,
                                    },
                                )
                                .await?
                            }
                            _ => {}
                        }
                        if !running {
                            break;
                        }
                    }
                    maybe_frame = frame_rx.recv() => {
                        match maybe_frame {
                            None => {
                                stream_error = Some(AppError::Protocol(
                                    "daemon closed the connection".to_string(),
                                ));
                                break;
                            }
                            Some(Err(err)) => {
                                stream_error = Some(err);
                                break;
                            }
                            Some(Ok(response)) => {
                            // Drain every frame the daemon has already queued and
                            // concatenate the output chunks into one buffer. A burst of
                            // output (an echoed paste, a full-screen redraw) otherwise
                            // costs one blocking write plus one flush of the unbuffered
                            // stdout handle per frame, which is what makes a large
                            // paste visibly crawl across the screen.
                            let mut batch = Vec::new();
                            let mut response = Some(response);
                            while let Some(current) = response.take() {
                                match current {
                                    RpcResponse::AttachStreamChunk { offset, data } => {
                                        if let Err(err) =
                                            stream_cursor.accept(offset, data.len())
                                        {
                                            stream_error = Some(err);
                                            running = false;
                                            break;
                                        }
                                        if batch.is_empty() {
                                            batch = data;
                                        } else {
                                            batch.extend_from_slice(&data);
                                        }
                                    }
                                    RpcResponse::AttachModeChanged {
                                        app_cursor_keys,
                                        bracketed_paste_mode,
                                    } => {
                                        child_app_cursor_keys = app_cursor_keys;
                                        child_bracketed_paste_mode = bracketed_paste_mode;
                                    }
                                    RpcResponse::AttachResized { rows: _, cols: _ } => {
                                        // Another client resized the PTY.  We cannot
                                        // programmatically resize the terminal window (only the
                                        // screen buffer on Windows, which corrupts the display).
                                        // Instead, update last_sent_size to the actual terminal
                                        // size so that the dedup guard in Event::Resize prevents
                                        // echoing our unchanged dimensions back to the server.
                                        let (actual_cols, actual_rows) =
                                            terminal::size().unwrap_or((80, 24));
                                        last_sent_size = (actual_cols, actual_rows);
                                    }
                                    RpcResponse::AttachControlChanged { role } => {
                                        is_controller = role == "controller";
                                    }
                                    RpcResponse::AttachStreamDone { final_offset, .. } => {
                                        if let Err(err) = stream_cursor.finish(final_offset) {
                                            stream_error = Some(err);
                                        }
                                        running = false;
                                    }
                                    _ => {}
                                }

                                if !running || batch.len() >= MAX_BATCHED_FRAME_BYTES {
                                    break;
                                }
                                match frame_rx.try_recv() {
                                    Ok(Ok(next)) => response = Some(next),
                                    Ok(Err(err)) => {
                                        stream_error = Some(err);
                                        running = false;
                                    }
                                    Err(_) => break,
                                }
                            }

                            if !batch.is_empty() {
                                #[cfg(windows)]
                                write_bytes_to_stdout(&renderer.render_chunk(&batch))?;
                                #[cfg(not(windows))]
                                write_bytes_to_stdout(&batch)?;
                                maybe_send_ack(
                                    &mut write_half,
                                    &id_owned,
                                    stream_cursor.current(),
                                    &mut last_acked,
                                )
                                .await;
                            }
                        }
                    }
                }
            }
        }

        // The terminal-event reader thread may still be parked in a blocking
        // read; it exits with the process. Nothing may join it here.
        drop(event_reader);

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

fn spawn_attach_shutdown_listener() -> mpsc::UnboundedReceiver<()> {
    let (shutdown_tx, shutdown_rx) = mpsc::unbounded_channel();

    {
        let shutdown_tx = shutdown_tx.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = shutdown_tx.send(());
            }
        });
    }

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            let shutdown_tx = shutdown_tx.clone();
            tokio::spawn(async move {
                let _ = terminate.recv().await;
                let _ = shutdown_tx.send(());
            });
        }

        if let Ok(mut hangup) = signal(SignalKind::hangup()) {
            let shutdown_tx = shutdown_tx.clone();
            tokio::spawn(async move {
                let _ = hangup.recv().await;
                let _ = shutdown_tx.send(());
            });
        }
    }

    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_logoff, ctrl_shutdown};

        if let Ok(mut ctrl_break) = ctrl_break() {
            let shutdown_tx = shutdown_tx.clone();
            tokio::spawn(async move {
                let _ = ctrl_break.recv().await;
                let _ = shutdown_tx.send(());
            });
        }

        if let Ok(mut ctrl_close) = ctrl_close() {
            let shutdown_tx = shutdown_tx.clone();
            tokio::spawn(async move {
                let _ = ctrl_close.recv().await;
                let _ = shutdown_tx.send(());
            });
        }

        if let Ok(mut ctrl_logoff) = ctrl_logoff() {
            let shutdown_tx = shutdown_tx.clone();
            tokio::spawn(async move {
                let _ = ctrl_logoff.recv().await;
                let _ = shutdown_tx.send(());
            });
        }

        if let Ok(mut ctrl_shutdown) = ctrl_shutdown() {
            let shutdown_tx = shutdown_tx.clone();
            tokio::spawn(async move {
                let _ = ctrl_shutdown.recv().await;
                let _ = shutdown_tx.send(());
            });
        }
    }

    shutdown_rx
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

/// Bytes that land `seed` (daemon-rendered, `\n`-terminated scrollback rows)
/// in the terminal's scrollback and leave a blank visible screen for the
/// snapshot repaint.
///
/// Raw mode is active, so `\n` does not imply a carriage return and every
/// seeded row needs an explicit CRLF. `rows` padding newlines then scroll
/// every seeded row above the visible area — unlike ED 2 (`\x1b[2J`), whose
/// effect on scrollback varies between terminals, plain scrolling works
/// everywhere — and `\x1b[H` homes the cursor for the snapshot.
fn scrollback_seed_bytes(seed: &[u8], rows: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(seed.len() + usize::from(rows) + 8);
    for &byte in seed {
        if byte == b'\n' {
            out.push(b'\r');
        }
        out.push(byte);
    }
    // The seed must end fully scrolled off: the snapshot repaints the visible
    // screen afterwards, and rows still on screen would be duplicated by it.
    // A seed at least one screen tall has already scrolled itself off — its
    // last row's own line feed did the final scroll — so extra newlines would
    // only push blank rows between the seed and the snapshot in the client's
    // scrollback.  A shorter seed still needs a full screen of newlines to
    // guarantee the scroll-off regardless of where the cursor started.
    let seed_lines = seed.iter().filter(|&&byte| byte == b'\n').count();
    if seed_lines < usize::from(rows) {
        out.resize(out.len() + usize::from(rows), b'\n');
    }
    out.extend_from_slice(b"\x1b[H");
    out
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

fn normalize_paste_text(data: String) -> String {
    data.replace("\r\n", "\n").replace('\r', "\n")
}

async fn maybe_collect_clipboard_paste(
    config: &AppConfig,
    id: &str,
    node: Option<&str>,
    key: KeyEvent,
    bracketed_paste_mode: bool,
) -> Result<Option<String>> {
    if !is_clipboard_paste_key(key) {
        return Ok(None);
    }

    let data = match node {
        Some(node) => handle_remote_clipboard_paste(config, id, node).await?,
        None => clipboard::handle_clipboard_paste(config, id, true)?,
    };

    Ok(data
        .map(normalize_paste_text)
        .map(|data| wrap_paste_input(data, bracketed_paste_mode)))
}

async fn handle_remote_clipboard_paste(
    config: &AppConfig,
    id: &str,
    node: &str,
) -> Result<Option<String>> {
    match clipboard::collect_remote_clipboard_transfer(true)? {
        Some(clipboard::RemoteClipboardTransfer::Text(text)) => Ok(Some(text)),
        Some(clipboard::RemoteClipboardTransfer::Files(files)) => {
            let mut uploaded_paths = Vec::with_capacity(files.len());
            for file in files {
                uploaded_paths.push(upload_remote_clipboard_file(config, id, node, file).await?);
            }
            Ok(Some(uploaded_paths.join("\n")))
        }
        None => Ok(None),
    }
}

async fn upload_remote_clipboard_file(
    config: &AppConfig,
    id: &str,
    node: &str,
    file: clipboard::RemoteClipboardFile,
) -> Result<String> {
    let request = RpcRequest::NodeProxy {
        node: node.to_string(),
        inner: Box::new(RpcRequest::UploadFile {
            id: id.to_string(),
            path: file.name,
            bytes: file.bytes,
            dedupe: true,
        }),
    };

    match ipc::send_request_checked(config, request).await? {
        RpcResponse::UploadFile { path, .. } => Ok(path),
        _ => Err(AppError::Protocol("unexpected response type".to_string())),
    }
}

fn is_clipboard_paste_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('\u{16}'))
        || (key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V')))
        || (key.modifiers.contains(KeyModifiers::SHIFT) && matches!(key.code, KeyCode::Insert))
}

// ---------------------------------------------------------------------------
// Key input mapping
// ---------------------------------------------------------------------------

/// Encode a key event into the bytes an xterm-compatible terminal would
/// send (ADR-0003 legacy profile; enhanced keyboard profiles such as the
/// kitty protocol are a separate negotiated capability and never enabled
/// implicitly).
///
/// Coverage contract: no common key is silently dropped. Modifier
/// combinations use xterm's parameterized forms; Alt on byte-like keys is
/// an ESC prefix; the Ctrl+@/digit family sends its legacy control bytes.
fn map_key_to_input(key: KeyEvent, app_cursor_keys: bool) -> Option<String> {
    let mods = key.modifiers;
    let alt = mods.contains(KeyModifiers::ALT);
    let shift = mods.contains(KeyModifiers::SHIFT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);

    // xterm's modifier parameter: 1 + Shift(1) + Alt(2) + Ctrl(4).
    let modifier_param = || -> Option<u8> {
        let mut value = 1u8;
        if shift {
            value += 1;
        }
        if alt {
            value += 2;
        }
        if ctrl {
            value += 4;
        }
        (value > 1).then_some(value)
    };

    // Alt on byte-like keys is an ESC prefix.
    let alt_prefix = |data: &str| -> String {
        if alt {
            format!("\x1b{data}")
        } else {
            data.to_string()
        }
    };

    match key.code {
        KeyCode::Enter => Some(alt_prefix("\r")),
        KeyCode::Tab if shift => Some(alt_prefix("\x1b[Z")),
        KeyCode::Tab => Some(alt_prefix("\t")),
        KeyCode::BackTab => Some(alt_prefix("\x1b[Z")),
        KeyCode::Backspace if ctrl => {
            // Ctrl+Backspace → ASCII BS (0x08) for apps that do not handle DEL.
            Some(alt_prefix("\x08"))
        }
        KeyCode::Backspace => Some(alt_prefix("\x7f")),
        KeyCode::Esc => Some(if alt {
            "\x1b\x1b".to_string()
        } else {
            "\x1b".to_string()
        }),
        KeyCode::Up | KeyCode::Down | KeyCode::Right | KeyCode::Left => {
            let letter = match key.code {
                KeyCode::Up => 'A',
                KeyCode::Down => 'B',
                KeyCode::Right => 'C',
                _ => 'D',
            };
            // Modified arrows are always CSI 1;{mod}{letter} — including
            // under DECCKM; only the unmodified forms honor the SS3 mode.
            if let Some(param) = modifier_param() {
                Some(format!("\x1b[1;{param}{letter}"))
            } else if app_cursor_keys {
                Some(format!("\x1bO{letter}"))
            } else {
                Some(format!("\x1b[{letter}"))
            }
        }
        KeyCode::Home | KeyCode::End => {
            let letter = if matches!(key.code, KeyCode::Home) {
                'H'
            } else {
                'F'
            };
            if let Some(param) = modifier_param() {
                Some(format!("\x1b[1;{param}{letter}"))
            } else {
                Some(format!("\x1b[{letter}"))
            }
        }
        KeyCode::Delete | KeyCode::Insert | KeyCode::PageUp | KeyCode::PageDown => {
            let number = match key.code {
                KeyCode::Insert => 2,
                KeyCode::Delete => 3,
                KeyCode::PageUp => 5,
                _ => 6,
            };
            if let Some(param) = modifier_param() {
                Some(format!("\x1b[{number};{param}~"))
            } else {
                Some(format!("\x1b[{number}~"))
            }
        }
        KeyCode::F(n) => {
            // F1–F4 use SS3 final bytes unmodified and CSI 1;{mod}{P..S}
            // modified; F5+ use the tilde forms.
            let ss3_letter = match n {
                1 => Some('P'),
                2 => Some('Q'),
                3 => Some('R'),
                4 => Some('S'),
                _ => None,
            };
            if let Some(letter) = ss3_letter {
                if let Some(param) = modifier_param() {
                    Some(format!("\x1b[1;{param}{letter}"))
                } else {
                    Some(format!("\x1bO{letter}"))
                }
            } else {
                let number = match n {
                    5 => 15,
                    6 => 17,
                    7 => 18,
                    8 => 19,
                    9 => 20,
                    10 => 21,
                    11 => 23,
                    _ => 24,
                };
                if let Some(param) = modifier_param() {
                    Some(format!("\x1b[{number};{param}~"))
                } else {
                    Some(format!("\x1b[{number}~"))
                }
            }
        }
        KeyCode::Char(c) => {
            if ctrl {
                let lower = c.to_ascii_lowercase();
                // The Ctrl+@/digit family carries legacy control bytes in
                // every mainstream terminal (NUL, ESC, FS, GS, RS, US, DEL);
                // `ch & 0x1f` on digit glyphs would send the wrong bytes.
                let legacy = match lower {
                    '2' | ' ' => Some('\0'),
                    '3' | '[' => Some('\x1b'),
                    '4' | '\\' => Some('\x1c'),
                    '5' | ']' => Some('\x1d'),
                    '6' | '^' | '~' => Some('\x1e'),
                    '7' | '_' => Some('\x1f'),
                    '8' => Some('\x7f'),
                    _ => None,
                };
                if let Some(byte) = legacy {
                    return Some(alt_prefix(&byte.to_string()));
                }
                if !c.is_ascii() {
                    return None;
                }
                let byte = (lower as u8) & 0x1f;
                Some(alt_prefix(&(byte as char).to_string()))
            } else if alt {
                Some(format!("\x1b{c}"))
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

/// Applied-cursor credit cadence (M3-5, I7): credits are backpressure
/// signals, not per-chunk chatter, so they go out at most once per MiB of
/// newly applied output.
const ACK_STRIDE_BYTES: u64 = 1024 * 1024;

/// Send an applied-cursor credit if the cursor advanced past the stride
/// since the last credit. Best-effort: send failures are ignored.
async fn maybe_send_ack(
    writer: &mut tokio::io::WriteHalf<interprocess::local_socket::tokio::Stream>,
    id: &str,
    cursor: u64,
    last_acked: &mut u64,
) {
    if cursor >= *last_acked + ACK_STRIDE_BYTES {
        let _ = ipc::write_request_to_writer(
            writer,
            RpcRequest::AttachAppliedCursor {
                id: id.to_string(),
                cursor,
            },
        )
        .await;
        *last_acked = cursor;
    }
}

fn is_ctrl_t(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('t') | KeyCode::Char('T'))
}

fn is_ctrl_d(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    #[test]
    fn scrollback_seed_uses_crlf_and_scrolls_every_seeded_row_into_history() {
        let bytes = scrollback_seed_bytes(b"line one\nline two\n\x1b[0m", 3);
        assert_eq!(
            bytes,
            b"line one\r\nline two\r\n\x1b[0m\n\n\n\x1b[H".as_slice()
        );
    }

    #[test]
    fn scrollback_seed_leaves_blank_screen_without_ed2() {
        let bytes = scrollback_seed_bytes(b"only\n", 2);
        assert!(!bytes.windows(4).any(|window| window == b"\x1b[2J"));
        assert!(bytes.ends_with(b"\x1b[H"));
    }

    #[test]
    fn scrollback_seed_taller_than_screen_needs_no_extra_newlines() {
        // A deep seed scrolls itself off with its own line feeds; extra
        // newlines would push blank rows between the seed and the snapshot
        // repaint in the client's scrollback.
        let seed = "row 0\nrow 1\nrow 2\n";
        let bytes = scrollback_seed_bytes(seed.as_bytes(), 3);
        assert_eq!(bytes, b"row 0\r\nrow 1\r\nrow 2\r\n\x1b[H".as_slice());
    }

    #[test]
    fn scrollback_seed_one_line_short_of_screen_still_scrolls_off() {
        // Short seeds keep the full screen of trailing newlines: the seed must
        // scroll off no matter where the cursor started.
        let seed = "row 0\nrow 1\n";
        let bytes = scrollback_seed_bytes(seed.as_bytes(), 3);
        assert_eq!(bytes, b"row 0\r\nrow 1\r\n\n\n\n\x1b[H".as_slice());
    }

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

    #[test]
    #[cfg(windows)]
    fn windows_live_chunk_is_repainted_from_canonical_screen_state() {
        let mut renderer = AttachRenderer::new(4, 20);
        let initial = renderer.render_initial(b"\x1b[2;5Hbefore");
        let mut replay = vt100::Parser::new(4, 20, 0);
        replay.process(&initial);

        let mut redraw = b"\x1b[H".to_vec();
        redraw.extend_from_slice(&vec![b' '; 25]);
        redraw.extend_from_slice("工作目录".as_bytes());

        let rendered = renderer.render_chunk(&redraw);

        assert!(rendered.starts_with(b"\x1b[?2026h"));
        assert!(rendered.ends_with(b"\x1b[?2026l"));
        replay.process(&rendered);
        assert_eq!(
            replay.screen().contents(),
            renderer.parser.screen().contents()
        );
        assert!(
            !rendered
                .windows(b"\x1b[2J".len())
                .any(|window| window == b"\x1b[2J")
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_live_chunk_forwards_title_and_progress_signals() {
        let mut renderer = AttachRenderer::new(4, 20);
        let _ = renderer.render_initial(b"\x1b[H");

        let rendered =
            renderer.render_chunk(b"\x1b]0;relay build\x07\x1b]9;4;3;0\x07\x1b[Hworking");

        let expected_signals = b"\x1b]0;relay build\x07\x1b]9;4;3;0\x07";
        assert!(rendered.starts_with(expected_signals));
        assert!(rendered[expected_signals.len()..].starts_with(b"\x1b[?2026h"));
        assert!(rendered.ends_with(b"\x1b[?2026l"));
        assert!(renderer.parser.screen().contents().contains("working"));
    }

    #[test]
    #[cfg(windows)]
    fn windows_title_only_chunk_is_forwarded_without_repaint() {
        let mut renderer = AttachRenderer::new(4, 20);
        let _ = renderer.render_initial(b"\x1b[Hidle");

        let rendered = renderer.render_chunk(b"\x1b]2;relay\x07");

        assert_eq!(rendered, b"\x1b]2;relay\x07".to_vec());
    }

    #[test]
    #[cfg(windows)]
    fn windows_live_chunk_forwards_cursor_shape_changes() {
        // The canonical screen state does not model DECSCUSR, so an editor's
        // bar cursor would be lost on every repaint without this passthrough.
        let mut renderer = AttachRenderer::new(4, 20);
        let _ = renderer.render_initial(b"\x1b[H");

        let rendered = renderer.render_chunk(b"\x1b[6 q\x1b[Hediting");

        assert!(rendered.starts_with(b"\x1b[6 q"));
        assert!(renderer.parser.screen().contents().contains("editing"));
    }

    #[test]
    #[cfg(windows)]
    fn windows_initial_snapshot_forwards_restored_signals() {
        let mut renderer = AttachRenderer::new(4, 20);

        let rendered = renderer.render_initial(b"\x1b[Hready\x1b]0;relay\x07\x1b[6 q");

        assert!(rendered.starts_with(b"\x1b]0;relay\x07\x1b[6 q"));
        assert!(renderer.parser.screen().contents().contains("ready"));
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

    #[test]
    fn test_normalize_paste_text_converts_crlf_to_lf() {
        assert_eq!(
            normalize_paste_text("line1\r\nline2\r\nline3".to_string()),
            "line1\nline2\nline3"
        );
    }

    #[test]
    fn test_normalize_paste_text_converts_lone_cr_to_lf() {
        assert_eq!(
            normalize_paste_text("line1\rline2\rline3".to_string()),
            "line1\nline2\nline3"
        );
    }

    #[test]
    fn test_is_clipboard_paste_key_accepts_ctrl_v_as_control_character() {
        assert!(is_clipboard_paste_key(ctrl_press(KeyCode::Char('\u{16}'))));
    }

    #[test]
    fn test_is_clipboard_paste_key_accepts_bare_control_character() {
        assert!(is_clipboard_paste_key(press(KeyCode::Char('\u{16}'))));
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

    // -------------------------------------------------------------------
    // M0 input-codec evidence (PLAN.md §8, ADR-0003). Passing tests pin
    // the incumbent baseline; ignored repros pin the complete, standard
    // encoding the 1.0 raw/semantic codec must provide.
    // -------------------------------------------------------------------

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn map_key_baseline_documents_current_coverage() {
        assert_eq!(
            map_key_to_input(key(KeyCode::Char('a'), KeyModifiers::NONE), false),
            Some("a".into())
        );
        assert_eq!(
            map_key_to_input(key(KeyCode::Enter, KeyModifiers::NONE), false),
            Some("\r".into())
        );
        assert_eq!(
            map_key_to_input(key(KeyCode::Backspace, KeyModifiers::CONTROL), false),
            Some("\x08".into())
        );
        assert_eq!(
            map_key_to_input(key(KeyCode::Backspace, KeyModifiers::NONE), false),
            Some("\x7f".into())
        );
        assert_eq!(
            map_key_to_input(key(KeyCode::Char('c'), KeyModifiers::CONTROL), false),
            Some("\x03".into())
        );
        assert_eq!(
            map_key_to_input(key(KeyCode::Left, KeyModifiers::NONE), true),
            Some("\x1bOD".into())
        );
    }

    /// Legacy/xterm-compatible profile (ADR-0001 stable profiles): the
    /// encoding an xterm-class terminal sends. Enhanced keyboard profiles
    /// (kitty protocol) are a separate negotiated capability.
    #[test]
    fn repro_alt_char_is_esc_prefixed() {
        assert_eq!(
            map_key_to_input(key(KeyCode::Char('x'), KeyModifiers::ALT), false),
            Some("\x1bx".to_string())
        );
    }

    #[test]
    fn repro_ctrl_arrow_is_parameterized() {
        assert_eq!(
            map_key_to_input(key(KeyCode::Up, KeyModifiers::CONTROL), false),
            Some("\x1b[1;5A".to_string())
        );
    }

    #[test]
    fn repro_ctrl_digit_family_sends_legacy_control_bytes() {
        // Ctrl+2 through Ctrl+8 duplicate the C0 control characters in
        // every mainstream terminal (NUL, ESC, FS, GS, RS, US, DEL).
        for (digit, expected) in [
            ('2', '\0'),
            ('3', '\x1b'),
            ('4', '\x1c'),
            ('5', '\x1d'),
            ('6', '\x1e'),
            ('7', '\x1f'),
            ('8', '\x7f'),
        ] {
            assert_eq!(
                map_key_to_input(key(KeyCode::Char(digit), KeyModifiers::CONTROL), false),
                Some(expected.to_string()),
                "Ctrl+{digit}"
            );
        }
    }

    #[test]
    fn repro_function_keys_are_mapped() {
        assert_eq!(
            map_key_to_input(key(KeyCode::F(5), KeyModifiers::NONE), false),
            Some("\x1b[15~".to_string())
        );
    }

    #[test]
    fn alt_prefixes_byte_like_keys() {
        assert_eq!(
            map_key_to_input(key(KeyCode::Enter, KeyModifiers::ALT), false),
            Some("\x1b\r".to_string())
        );
        assert_eq!(
            map_key_to_input(key(KeyCode::Tab, KeyModifiers::ALT), false),
            Some("\x1b\t".to_string())
        );
        assert_eq!(
            map_key_to_input(key(KeyCode::Esc, KeyModifiers::ALT), false),
            Some("\x1b\x1b".to_string())
        );
        assert_eq!(
            map_key_to_input(key(KeyCode::Backspace, KeyModifiers::ALT), false),
            Some("\x1b\x7f".to_string())
        );
    }

    #[test]
    fn modifiers_parameterize_home_delete_page_keys() {
        assert_eq!(
            map_key_to_input(key(KeyCode::Home, KeyModifiers::SHIFT), false),
            Some("\x1b[1;2H".to_string())
        );
        assert_eq!(
            map_key_to_input(key(KeyCode::Delete, KeyModifiers::CONTROL), false),
            Some("\x1b[3;5~".to_string())
        );
        assert_eq!(
            map_key_to_input(key(KeyCode::PageUp, KeyModifiers::ALT), false),
            Some("\x1b[5;3~".to_string())
        );
        assert_eq!(
            map_key_to_input(
                key(KeyCode::End, KeyModifiers::SHIFT | KeyModifiers::CONTROL),
                false
            ),
            Some("\x1b[1;6F".to_string())
        );
    }

    #[test]
    fn modified_arrows_ignore_app_cursor_keys_mode() {
        // DECCKM selects the SS3 form only for unmodified arrows; modified
        // arrows are always CSI 1;{mod}{letter} (xterm behavior).
        assert_eq!(
            map_key_to_input(key(KeyCode::Left, KeyModifiers::CONTROL), true),
            Some("\x1b[1;5D".to_string())
        );
        assert_eq!(
            map_key_to_input(key(KeyCode::Up, KeyModifiers::NONE), true),
            Some("\x1bOA".to_string())
        );
    }

    #[test]
    fn function_keys_cover_f1_through_f12_with_modifiers() {
        let unmodified = [
            "\x1bOP",   // F1
            "\x1bOQ",   // F2
            "\x1bOR",   // F3
            "\x1bOS",   // F4
            "\x1b[15~", // F5
            "\x1b[17~", // F6
            "\x1b[18~", // F7
            "\x1b[19~", // F8
            "\x1b[20~", // F9
            "\x1b[21~", // F10
            "\x1b[23~", // F11
            "\x1b[24~", // F12
        ];
        for (n, expected) in (1u8..=12).zip(unmodified) {
            assert_eq!(
                map_key_to_input(key(KeyCode::F(n), KeyModifiers::NONE), false),
                Some(expected.to_string()),
                "F{n}"
            );
        }
        // Modified low F-keys switch to the parameterized CSI form,
        // modified high F-keys add the modifier parameter.
        assert_eq!(
            map_key_to_input(key(KeyCode::F(1), KeyModifiers::SHIFT), false),
            Some("\x1b[1;2P".to_string())
        );
        assert_eq!(
            map_key_to_input(
                key(KeyCode::F(5), KeyModifiers::CONTROL | KeyModifiers::SHIFT),
                false
            ),
            Some("\x1b[15;6~".to_string())
        );
        assert_eq!(
            map_key_to_input(key(KeyCode::F(12), KeyModifiers::ALT), false),
            Some("\x1b[24;3~".to_string())
        );
    }
}
