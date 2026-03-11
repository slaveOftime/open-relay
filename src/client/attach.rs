use std::io::Write;

use crossterm::{
    cursor,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers,
    },
    execute, terminal,
};
use tokio::io::BufReader;

use crate::{
    config::AppConfig,
    error::{AppError, Result},
    ipc,
    protocol::{RpcRequest, RpcResponse},
    utils::{
        TerminalQuery, find_next_terminal_query, terminal_query_response, terminal_query_tail_len,
    },
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
    // For node-proxied attaches we fall back to the old polling model since
    // the streaming protocol goes over the direct local socket only.
    if node.is_some() {
        return run_attach_polled(config, id, node).await;
    }

    let stream = ipc::connect(config).await?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    // Send AttachSubscribe.
    ipc::write_request_to_writer(
        &mut write_half,
        RpcRequest::AttachSubscribe {
            id: id.to_string(),
            from_byte_offset: None,
        },
    )
    .await?;

    // Receive the init frame.
    let init = ipc::read_response_from_reader(&mut reader).await?;
    let (initial_lines, mut running, mut child_app_cursor_keys) = match init {
        RpcResponse::AttachStreamInit {
            lines,
            running,
            app_cursor_keys,
            ..
        } => (lines, running, app_cursor_keys),
        RpcResponse::Error { message } => return Err(AppError::DaemonUnavailable(message)),
        _ => return Err(AppError::Protocol("unexpected response type".to_string())),
    };

    let mut detached = false;
    let mut stream_error: Option<AppError> = None;
    {
        let _raw_mode = RawModeGuard::new()?;

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

        let mut query_tail = String::new();
        let mut response_tail = String::new();
        render_lines(
            config,
            id,
            initial_lines,
            &mut query_tail,
            &mut response_tail,
            None,
        )
        .await?;

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
            let frame = tokio::time::timeout(
                std::time::Duration::from_millis(60),
                ipc::read_response_from_reader(&mut reader),
            )
            .await;
            match frame {
                Err(_timeout) => continue,
                Ok(Err(err)) => {
                    stream_error = Some(err);
                    break;
                }
                Ok(Ok(RpcResponse::AttachStreamChunk { data, .. })) => {
                    let text = String::from_utf8_lossy(&data);
                    let chunk = String::from(&*text);
                    respond_to_terminal_queries(
                        config,
                        id,
                        &mut query_tail,
                        &mut response_tail,
                        &chunk,
                        None,
                    )
                    .await?;
                    std::io::stdout().flush()?;
                }
                Ok(Ok(RpcResponse::AttachModeChanged {
                    app_cursor_keys, ..
                })) => {
                    child_app_cursor_keys = app_cursor_keys;
                }
                Ok(Ok(RpcResponse::AttachStreamDone { .. })) => {
                    running = false;
                }
                Ok(Ok(RpcResponse::Error { message })) => {
                    return Err(AppError::DaemonUnavailable(message));
                }
                Ok(Ok(_)) => {}
            }
        }
    }

    if detached {
        // Send detach signal to daemon.
        let _ = ipc::write_request_to_writer(
            &mut write_half,
            RpcRequest::AttachDetach { id: id.to_string() },
        )
        .await;
        println!("\r\nDetached from session {id}");
    } else if let Some(err) = stream_error {
        eprintln!("\r\nAttach session {id} ended with error: {err}");
    } else {
        println!("\r\nSession {id} has ended.");
    }

    Ok(())
}

/// Fallback polling-based attach used when routing through a node proxy.
async fn run_attach_polled(config: &AppConfig, id: &str, node: Option<&str>) -> Result<()> {
    let snapshot = ipc::send_request(
        config,
        attach_proxy(
            node,
            RpcRequest::AttachSubscribe {
                id: id.to_string(),
                from_byte_offset: None,
            },
        ),
    )
    .await?;

    let (initial_lines, mut running, child_app_cursor_keys) = match snapshot {
        RpcResponse::AttachStreamInit {
            lines,
            running,
            app_cursor_keys,
            ..
        } => (lines, running, app_cursor_keys),
        RpcResponse::Error { message } => return Err(AppError::DaemonUnavailable(message)),
        _ => return Err(AppError::Protocol("unexpected response type".to_string())),
    };

    let mut detached = false;
    {
        let _raw_mode = RawModeGuard::new()?;
        send_resize(config, id, node).await?;
        let mut query_tail = String::new();
        let mut response_tail = String::new();
        render_lines(
            config,
            id,
            initial_lines,
            &mut query_tail,
            &mut response_tail,
            node,
        )
        .await?;
        let mut saw_ctrl_bracket = false;

        while running {
            while event::poll(std::time::Duration::from_millis(0))? {
                match event::read()? {
                    Event::Paste(data) => send_input(config, id, data, node).await?,
                    Event::Resize(_, _) => send_resize(config, id, node).await?,
                    Event::Key(key) => {
                        if !matches!(key.kind, KeyEventKind::Press) {
                            continue;
                        }
                        if is_ctrl_bracket(key) {
                            saw_ctrl_bracket = true;
                            continue;
                        }
                        if is_ctrl_d(key)
                            || (saw_ctrl_bracket
                                && matches!(
                                    key.code,
                                    KeyCode::Char('c')
                                        | KeyCode::Char('C')
                                        | KeyCode::Char('d')
                                        | KeyCode::Char('D')
                                ))
                        {
                            detached = true;
                            running = false;
                            break;
                        }
                        saw_ctrl_bracket = false;
                        if let Some(data) = map_key_to_input(key, child_app_cursor_keys) {
                            send_input(config, id, data, node).await?;
                        }
                    }
                    _ => {}
                }
            }

            if !running {
                break;
            }

            // Node-proxied quick poll using logs_snapshot.
            let response = ipc::send_request(
                config,
                attach_proxy(
                    node,
                    RpcRequest::LogsSnapshot {
                        id: id.to_string(),
                        tail: 50,
                    },
                ),
            )
            .await?;

            match response {
                RpcResponse::LogsSnapshot {
                    lines,
                    running: next_running,
                    ..
                } => {
                    let new_lines: Vec<String> = lines;
                    render_lines(
                        config,
                        id,
                        new_lines,
                        &mut query_tail,
                        &mut response_tail,
                        node,
                    )
                    .await?;
                    running = next_running;
                }
                RpcResponse::Error { message } => return Err(AppError::DaemonUnavailable(message)),
                _ => return Err(AppError::Protocol("unexpected response type".to_string())),
            }

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    if detached {
        send_detach(config, id, node).await?;
        println!("\r\nDetached from session {id}");
    } else {
        println!("\r\nSession {id} has ended.");
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
            cursor::Hide,
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
        let execute_result = if self.alternate_screen {
            execute!(
                stdout,
                DisableBracketedPaste,
                DisableMouseCapture,
                terminal::LeaveAlternateScreen,
                cursor::Show,
                cursor::MoveToColumn(0)
            )
        } else {
            execute!(
                stdout,
                DisableBracketedPaste,
                cursor::Show,
                cursor::MoveToColumn(0)
            )
        };

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

async fn render_lines(
    config: &AppConfig,
    id: &str,
    lines: Vec<String>,
    query_tail: &mut String,
    response_tail: &mut String,
    node: Option<&str>,
) -> Result<()> {
    for line in lines {
        respond_to_terminal_queries(config, id, query_tail, response_tail, &line, node).await?;
    }
    std::io::stdout().flush()?;
    Ok(())
}

/// Detects and responds to CPR / DSR terminal queries that the child process
/// may emit (e.g. readline uses them to detect terminal capabilities).
async fn respond_to_terminal_queries(
    config: &AppConfig,
    id: &str,
    tail: &mut String,
    response_tail: &mut String,
    chunk: &str,
    node: Option<&str>,
) -> Result<()> {
    let mut combined = String::with_capacity(tail.len() + chunk.len());
    combined.push_str(tail);
    combined.push_str(chunk);

    let mut search_from = 0usize;
    while search_from < combined.len() {
        let Some((match_start, query_len, query)) =
            find_next_terminal_query(&combined, search_from)
        else {
            break;
        };

        if match_start > search_from {
            print_sanitized_output(response_tail, &combined[search_from..match_start]);
            std::io::stdout().flush()?;
        }

        respond_to_terminal_query(config, id, query, node).await?;

        search_from = match_start + query_len;
    }

    let remainder = &combined[search_from..];
    let keep = terminal_query_tail_len(remainder);

    let printable_len = remainder.len().saturating_sub(keep);
    if printable_len > 0 {
        print_sanitized_output(response_tail, &remainder[..printable_len]);
    }
    *tail = remainder[printable_len..].to_string();

    Ok(())
}

fn print_sanitized_output(response_tail: &mut String, chunk: &str) {
    let sanitized = filter_terminal_response_chunk(response_tail, chunk);
    if !sanitized.is_empty() {
        print!("{sanitized}");
    }
}

fn filter_terminal_response_chunk(pending: &mut String, chunk: &str) -> String {
    use std::sync::OnceLock;

    static PARTIAL_CPR_RE: OnceLock<regex::Regex> = OnceLock::new();
    let partial_cpr_re = PARTIAL_CPR_RE
        .get_or_init(|| regex::Regex::new(r"\x1b(?:\[(?:\??\d*(?:;\d*)?)?)?$").unwrap());

    static FULL_CPR_RE: OnceLock<regex::Regex> = OnceLock::new();
    let full_cpr_re = FULL_CPR_RE.get_or_init(|| regex::Regex::new(r"\x1b\[\??\d+;\d+R").unwrap());

    static BARE_CPR_RE: OnceLock<regex::Regex> = OnceLock::new();
    let bare_cpr_re = BARE_CPR_RE.get_or_init(|| regex::Regex::new(r"\[\??\d+;\d+R").unwrap());

    static FULL_OSC_RE: OnceLock<regex::Regex> = OnceLock::new();
    let full_osc_re = FULL_OSC_RE.get_or_init(|| {
        regex::Regex::new(
            r"\x1b]1(?:0|1);rgb:[0-9A-Fa-f]{4}/[0-9A-Fa-f]{4}/[0-9A-Fa-f]{4}(?:\x07|\x1b\\)",
        )
        .unwrap()
    });

    static BARE_OSC_RE: OnceLock<regex::Regex> = OnceLock::new();
    let bare_osc_re = BARE_OSC_RE.get_or_init(|| {
        regex::Regex::new(r"]1(?:0|1);rgb:[0-9A-Fa-f]{4}/[0-9A-Fa-f]{4}/[0-9A-Fa-f]{4}(?:\x07|\\)")
            .unwrap()
    });

    let mut combined = std::mem::take(pending);
    combined.push_str(chunk);

    let cpr_pending_start = partial_cpr_re.find(&combined).map(|m| m.start());
    let osc_pending_start = trailing_partial_osc_response_start(&combined);
    if let Some(start) = [cpr_pending_start, osc_pending_start]
        .into_iter()
        .flatten()
        .min()
    {
        *pending = combined[start..].to_string();
        combined.truncate(start);
    }

    let combined = full_cpr_re.replace_all(&combined, "");
    let combined = bare_cpr_re.replace_all(&combined, "");
    let combined = full_osc_re.replace_all(&combined, "");
    bare_osc_re.replace_all(&combined, "").into_owned()
}

fn trailing_partial_osc_response_start(text: &str) -> Option<usize> {
    ["\x1b]10;rgb:", "\x1b]11;rgb:", "]10;rgb:", "]11;rgb:"]
        .into_iter()
        .filter_map(|prefix| text.rfind(prefix))
        .filter(|start| is_partial_osc_color_response(&text[*start..]))
        .min()
}

fn is_partial_osc_color_response(candidate: &str) -> bool {
    let bytes = candidate.as_bytes();
    let mut index = 0usize;

    if bytes.first() == Some(&0x1b) {
        index += 1;
    }

    if bytes.get(index) != Some(&b']') {
        return false;
    }
    index += 1;

    let rest = &candidate[index..];
    let prefix_len = if rest.starts_with("10;rgb:") || rest.starts_with("11;rgb:") {
        7
    } else {
        return false;
    };
    index += prefix_len;

    let mut slash_count = 0usize;
    let mut hex_in_component = 0usize;
    let mut saw_hex = false;

    while let Some(&byte) = bytes.get(index) {
        if byte.is_ascii_hexdigit() {
            if hex_in_component == 4 {
                return false;
            }
            hex_in_component += 1;
            saw_hex = true;
            index += 1;
            continue;
        }

        match byte {
            b'/' => {
                if !saw_hex || slash_count >= 2 {
                    return false;
                }
                slash_count += 1;
                hex_in_component = 0;
                saw_hex = false;
                index += 1;
            }
            0x07 => return slash_count == 2 && saw_hex,
            0x1b => return index + 1 == bytes.len(),
            b'\\' => return false,
            _ => return false,
        }
    }

    true
}

async fn respond_to_terminal_query(
    config: &AppConfig,
    id: &str,
    query: TerminalQuery,
    node: Option<&str>,
) -> Result<()> {
    let response = match query {
        TerminalQuery::CursorPositionReport => {
            let (col, row) = cursor::position().unwrap_or((0, 0));
            terminal_query_response(
                TerminalQuery::CursorPositionReport,
                Some((row.saturating_add(1), col.saturating_add(1))),
            )
        }
        _ => terminal_query_response(query, None),
    };

    send_input(config, id, response, node).await
}

pub async fn send_input(
    config: &AppConfig,
    id: &str,
    data: String,
    node: Option<&str>,
) -> Result<()> {
    match ipc::send_request(
        config,
        attach_proxy(
            node,
            RpcRequest::AttachInput {
                id: id.to_string(),
                data,
            },
        ),
    )
    .await?
    {
        RpcResponse::Ack => Ok(()),
        RpcResponse::Error { message } => Err(AppError::DaemonUnavailable(message)),
        _ => Err(AppError::Protocol("unexpected response type".to_string())),
    }
}

async fn send_resize(config: &AppConfig, id: &str, node: Option<&str>) -> Result<()> {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    match ipc::send_request(
        config,
        attach_proxy(
            node,
            RpcRequest::AttachResize {
                id: id.to_string(),
                rows,
                cols,
            },
        ),
    )
    .await?
    {
        RpcResponse::Ack => Ok(()),
        RpcResponse::Error { message } => Err(AppError::DaemonUnavailable(message)),
        _ => Err(AppError::Protocol("unexpected response type".to_string())),
    }
}

async fn send_detach(config: &AppConfig, id: &str, node: Option<&str>) -> Result<()> {
    match ipc::send_request(
        config,
        attach_proxy(node, RpcRequest::AttachDetach { id: id.to_string() }),
    )
    .await?
    {
        RpcResponse::Ack => Ok(()),
        RpcResponse::Error { message } => Err(AppError::DaemonUnavailable(message)),
        _ => Err(AppError::Protocol("unexpected response type".to_string())),
    }
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
        KeyCode::Backspace => Some("\x08".to_string()),
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

fn is_ctrl_bracket(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(']') | KeyCode::Char('5'))
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
            Some("\x08".to_string())
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
    // is_ctrl_bracket
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_ctrl_bracket_true_for_right_bracket() {
        assert!(is_ctrl_bracket(ctrl_press(KeyCode::Char(']'))));
    }

    #[test]
    fn test_is_ctrl_bracket_false_for_plain_char() {
        assert!(!is_ctrl_bracket(press(KeyCode::Char(']'))));
        assert!(!is_ctrl_bracket(ctrl_press(KeyCode::Char('c'))));
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
