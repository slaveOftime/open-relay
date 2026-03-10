use std::{
    collections::VecDeque,
    io::{ErrorKind, Read, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use uuid::Uuid;

use crate::{
    config::AppConfig,
    error::{AppError, Result},
};

use super::{
    SessionMeta, SessionStatus,
    persist::{append_event, append_output, append_resize_event},
};

// ---------------------------------------------------------------------------
// RuntimeChild
// ---------------------------------------------------------------------------

pub enum RuntimeChild {
    Pty(Box<dyn portable_pty::Child + Send + Sync>),
}

impl RuntimeChild {
    pub fn process_id(&self) -> Option<u32> {
        match self {
            Self::Pty(child) => child.process_id(),
        }
    }

    pub fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Pty(child) => child.kill(),
        }
    }

    pub fn try_wait_code(&mut self) -> std::io::Result<Option<i32>> {
        match self {
            Self::Pty(child) => child
                .try_wait()
                .map(|opt| opt.map(|status| status.exit_code() as i32)),
        }
    }
}

const ATTACH_ACTIVITY_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// SessionRuntime
// ---------------------------------------------------------------------------

pub struct SessionRuntime {
    pub meta: SessionMeta,
    pub dir: PathBuf,
    /// Total number of lines ever pushed to this session's output.
    /// Used as a cursor for incremental polling via `attach_poll` / `logs_poll`.
    pub output_line_count: usize,
    pub ring: VecDeque<String>,
    pub ring_limit: usize,
    pub writer: Box<dyn Write + Send>,
    pub child: RuntimeChild,
    pub completed_at: Option<Instant>,
    pub _pty_master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    /// Set to `true` once the completed state has been written to the database.
    pub persisted: bool,
    /// Timestamp of the last output chunk received; used by the notification engine.
    pub last_output_at: Option<Instant>,
    /// Timestamp of the last input bytes forwarded to the PTY; used to suppress
    /// notifications while the user is actively typing (input arrived after output).
    pub last_input_at: Option<Instant>,
    /// Timestamp of the last activity from an attach client, i.e. any API call that
    /// indicates the user is actively viewing/interacting with the session.
    /// Used to suppress notifications while the user is actively attached.
    pub last_attach_activity_at: Option<Instant>,
    /// Timestamp of the last *successful* notification delivery for this session.
    /// Useful for diagnostics/telemetry of notification behavior.
    pub last_notified_at: Option<Instant>,
    /// The value of `last_output_at` at the time of the last *successful* notification.
    /// Re-notification is suppressed until `last_output_at` advances past this epoch
    /// (i.e. new visible output has arrived).
    pub notified_output_epoch: Option<Instant>,
    /// Carries a trailing `\x1b` byte from the end of one PTY chunk to the start
    /// of the next. ConPTY on Windows sometimes splits `\x1b[N;NR` across two
    /// reads; by prepending the saved ESC we always see the complete sequence and
    /// can strip it without touching bare `[` characters in normal output.
    pub pending_cpr_prefix: String,
    /// Whether the child process has enabled bracketed-paste mode
    /// (`\x1b[?2004h`).  Updated by `push_output` scanning the PTY output
    /// stream, mirroring what xterm.js does via `decPrivateModes.bracketedPasteMode`.
    pub bracketed_paste_mode: bool,
    /// Carries trailing bytes that may be a partial DSR/CPR query
    /// (`\x1b[5n` / `\x1b[6n`) between PTY chunks for daemon-side fallback
    /// handling when no client is attached.
    pub pending_terminal_query_tail: String,
    /// Whether the child process has enabled application cursor key mode
    /// (DECCKM, `\x1b[?1h`). When active, arrow key sequences sent to the
    /// child must use `\x1bO{A,B,C,D}` instead of `\x1b[{A,B,C,D}`.
    pub app_cursor_keys: bool,
    pub notifications_enabled: bool,
}

impl SessionRuntime {
    pub fn push_output(&mut self, chunk: String) {
        let chunk = if self.has_active_attach_client() {
            if self.pending_terminal_query_tail.is_empty() {
                chunk
            } else {
                let mut merged = std::mem::take(&mut self.pending_terminal_query_tail);
                merged.push_str(&chunk);
                merged
            }
        } else {
            self.respond_to_terminal_queries_without_attach(&chunk)
        };

        let chunk = filter_cpr_chunk(&mut self.pending_cpr_prefix, &chunk);
        for line in chunk.split_inclusive('\n') {
            let normalized = line.to_string();
            self.output_line_count += 1;
            self.ring.push_back(normalized.clone());
            while self.ring.len() > self.ring_limit {
                let _ = self.ring.pop_front();
            }
            let _ = append_output(&self.dir, &normalized);
        }
        // Track whether the child has enabled/disabled bracketed-paste mode
        // (`\x1b[?2004h` = enable, `\x1b[?2004l` = disable). Mirrors what
        // xterm.js does so the attach client knows how to forward pastes.
        if chunk.contains("\x1b[?2004h") {
            self.bracketed_paste_mode = true;
        } else if chunk.contains("\x1b[?2004l") {
            self.bracketed_paste_mode = false;
        }
        // Track DECCKM (application cursor key mode, DEC private mode 1).
        // `\x1b[?1h` = enable, `\x1b[?1l` = disable.
        // When active, arrow keys must be sent as `\x1bO{A,B,C,D}` rather
        // than the standard-mode `\x1b[{A,B,C,D}`.
        if chunk.contains("\x1b[?1h") {
            self.app_cursor_keys = true;
        } else if chunk.contains("\x1b[?1l") {
            self.app_cursor_keys = false;
        }
        // Only advance the silence clock when the chunk contains visible characters.
        // Pure ANSI/control sequences (cursor moves, redraws) must NOT reset the
        // silence timer — interactive CLIs emit these continuously while waiting.
        if has_visible_content(&chunk) {
            // println!("| {}", chunk.replace('\x1b', "\\x1b"));
            self.last_output_at = Some(Instant::now());
        }
    }

    pub fn mark_attach_activity(&mut self) {
        self.last_attach_activity_at = Some(Instant::now());
    }

    fn has_active_attach_client(&self) -> bool {
        self.last_attach_activity_at
            .map(|t| Instant::now().duration_since(t) <= ATTACH_ACTIVITY_TIMEOUT)
            .unwrap_or(false)
    }

    fn respond_to_terminal_queries_without_attach(&mut self, chunk: &str) -> String {
        const CPR_QUERY: &str = "\x1b[6n";
        const DSR_QUERY: &str = "\x1b[5n";

        let mut combined = std::mem::take(&mut self.pending_terminal_query_tail);
        combined.push_str(chunk);

        let mut output = String::with_capacity(combined.len());
        let mut search_from = 0usize;

        while search_from < combined.len() {
            let cpr_match = combined[search_from..]
                .find(CPR_QUERY)
                .map(|o| (o, CPR_QUERY));
            let dsr_match = combined[search_from..]
                .find(DSR_QUERY)
                .map(|o| (o, DSR_QUERY));

            let Some((offset, query)) = [cpr_match, dsr_match]
                .into_iter()
                .flatten()
                .min_by_key(|(o, _)| *o)
            else {
                break;
            };

            let match_start = search_from + offset;
            if match_start > search_from {
                output.push_str(&combined[search_from..match_start]);
            }

            let response = match query {
                CPR_QUERY => "\x1b[1;1R",
                DSR_QUERY => "\x1b[0n",
                _ => "",
            };
            if !response.is_empty() {
                let _ = self.writer.write_all(response.as_bytes());
                let _ = self.writer.flush();
            }

            search_from = match_start + query.len();
        }

        let remainder = &combined[search_from..];
        let max_prefix = CPR_QUERY.len().max(DSR_QUERY.len()).saturating_sub(1);
        let mut keep = 0usize;
        for prefix_len in (1..=max_prefix).rev() {
            if remainder.ends_with(&CPR_QUERY[..prefix_len])
                || remainder.ends_with(&DSR_QUERY[..prefix_len])
            {
                keep = prefix_len;
                break;
            }
        }

        let printable_len = remainder.len().saturating_sub(keep);
        if printable_len > 0 {
            output.push_str(&remainder[..printable_len]);
        }
        self.pending_terminal_query_tail = remainder[printable_len..].to_string();

        output
    }

    /// Checks child exit status and updates `meta.status`. Returns `true` if completed.
    pub fn refresh_status(&mut self) -> bool {
        if self.is_completed() {
            if self.completed_at.is_none() {
                self.completed_at = Some(Instant::now());
            }
            return true;
        }

        match self.child.try_wait_code() {
            Ok(Some(code)) => {
                let status = if code == 0 {
                    SessionStatus::Stopped
                } else {
                    SessionStatus::Failed
                };
                self.mark_completed(status, Some(code));
                true
            }
            Ok(None) => {
                if !matches!(self.meta.status, SessionStatus::Stopping) {
                    self.meta.status = SessionStatus::Running;
                }
                false
            }
            Err(_) => {
                self.mark_completed(SessionStatus::Failed, None);
                true
            }
        }
    }

    pub fn mark_completed(&mut self, status: SessionStatus, exit_code: Option<i32>) {
        if self.meta.ended_at.is_none() {
            self.meta.ended_at = Some(chrono::Utc::now());
        }
        self.meta.status = status;
        if let Some(code) = exit_code {
            self.meta.exit_code = Some(code);
        }
        if self.completed_at.is_none() {
            self.completed_at = Some(Instant::now());
        }
        let event = match &self.meta.status {
            SessionStatus::Stopped => format!(
                "session stopped exit_code={}",
                self.meta.exit_code.unwrap_or(0)
            ),
            SessionStatus::Failed => format!(
                "session failed exit_code={}",
                self.meta.exit_code.unwrap_or(-1)
            ),
            other => format!("session ended status={}", other.as_str()),
        };
        let _ = append_event(&self.dir, &event);
    }

    pub fn is_completed(&self) -> bool {
        matches!(
            self.meta.status,
            SessionStatus::Stopped | SessionStatus::Failed
        )
    }

    pub fn resize_pty(&mut self, rows: u16, cols: u16) -> bool {
        if rows == 0 || cols == 0 {
            return false;
        }
        let Some(pty_master) = self._pty_master.as_mut() else {
            return false;
        };
        pty_master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .is_ok()
    }
}

// ---------------------------------------------------------------------------
// Session ID generation
// ---------------------------------------------------------------------------

pub fn generate_session_id<F: Fn(&str) -> bool>(exists: F) -> String {
    loop {
        let raw = Uuid::new_v4().as_simple().to_string();
        let candidate = raw.chars().take(7).collect::<String>();
        if !exists(&candidate) {
            return candidate;
        }
    }
}

// ---------------------------------------------------------------------------
// PTY spawning
// ---------------------------------------------------------------------------

/// Spawns a PTY-backed child process and returns an `Arc<Mutex<SessionRuntime>>`.
/// Reader threads are started automatically and share ownership via the same Arc.
/// `session_dir` is the absolute path for the session's working files; the caller
/// is responsible for computing it (typically `sessions_dir.join(&meta.id)`).
pub fn spawn_session(
    config: &AppConfig,
    meta: &mut SessionMeta,
    session_dir: PathBuf,
    rows: u16,
    cols: u16,
    notifications_enabled: bool,
) -> Result<Arc<Mutex<SessionRuntime>>> {
    let full_dir = session_dir;
    std::fs::create_dir_all(&full_dir)?;

    let Ok(cmd) = which::which(&meta.command) else {
        return Err(AppError::Protocol(format!(
            "command not found: {}",
            meta.command
        )));
    };

    let mut cmd = CommandBuilder::new(cmd);
    cmd.args(&meta.args);
    let cwd_fallback = full_dir.to_string_lossy().into_owned();
    cmd.cwd(meta.cwd.as_ref().unwrap_or(&cwd_fallback));

    let cmd_display = format_command_for_display(&meta.command, &meta.args);
    let pty = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| {
            AppError::Protocol(format!("failed to allocate PTY for `{cmd_display}`: {err}"))
        })?;

    let child = pty.slave.spawn_command(cmd).map_err(|err| {
        AppError::Protocol(format!(
            "failed to spawn `{cmd_display}` (cwd={}): {err}",
            meta.cwd.as_deref().unwrap_or("<current>")
        ))
    })?;

    let master = pty.master;
    let reader = master.try_clone_reader().map_err(|err| {
        AppError::Protocol(format!(
            "failed to create PTY reader for `{cmd_display}`: {err}"
        ))
    })?;
    let writer = master.take_writer().map_err(|err| {
        AppError::Protocol(format!(
            "failed to create PTY writer for `{cmd_display}`: {err}"
        ))
    })?;
    let runtime_child = RuntimeChild::Pty(child);
    meta.pid = runtime_child.process_id();

    std::fs::write(full_dir.join("output.log"), b"")?;
    std::fs::write(full_dir.join("events.log"), b"session created\n")?;
    let _ = append_resize_event(&full_dir, 0, rows, cols);
    let started_pid = meta
        .pid
        .map(|p| p.to_string())
        .unwrap_or_else(|| "?".to_string());
    let _ = append_event(&full_dir, &format!("session started pid={started_pid}"));

    let runtime = Arc::new(Mutex::new(SessionRuntime {
        meta: meta.clone(),
        dir: full_dir,
        output_line_count: 0,
        ring: VecDeque::new(),
        ring_limit: config.ring_buffer_lines,
        writer,
        child: runtime_child,
        completed_at: None,
        _pty_master: Some(master),
        persisted: false,
        last_output_at: None,
        last_input_at: None,
        last_attach_activity_at: None,
        notified_output_epoch: None,
        last_notified_at: None,
        pending_cpr_prefix: String::new(),
        bracketed_paste_mode: false,
        pending_terminal_query_tail: String::new(),
        app_cursor_keys: false,
        notifications_enabled,
    }));

    // Spawn reader thread that feeds PTY output into the runtime buffer.
    let runtime_reader = runtime.clone();
    std::thread::spawn(move || {
        if let Ok(rt) = runtime_reader.lock() {
            let _ = append_event(&rt.dir, "pty reader started");
        }
        let mut buf = [0u8; 4096];
        let mut reader = reader;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    if let Ok(rt) = runtime_reader.lock() {
                        let _ = append_event(&rt.dir, "pty reader reached EOF");
                    }
                    break;
                }
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                    match runtime_reader.lock() {
                        Ok(mut rt) => rt.push_output(chunk),
                        Err(_) => break,
                    }
                }
                Err(err)
                    if matches!(err.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => {
                    if let Ok(rt) = runtime_reader.lock() {
                        let _ = append_event(&rt.dir, &format!("pty reader error: {err}"));
                    }
                    break;
                }
            }
        }
    });

    Ok(runtime)
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn format_command_for_display(command: &str, args: &[String]) -> String {
    if args.is_empty() {
        return command.to_string();
    }
    format!("{} {}", command, args.join(" "))
}

/// Filters CPR (Cursor Position Report) sequences out of a single PTY chunk.
///
/// ConPTY on Windows echoes the CPR response (`\x1b[row;colR`) into the master
/// output stream.  The sequence is frequently split across PTY read boundaries
/// at *any* byte, so this function carries a `pending` prefix from one call to
/// the next to ensure every fragment is reassembled before being examined.
///
/// Splitting points handled (all stripped):
/// * Full sequence in one chunk: `\x1b[35;1R`
/// * ESC alone:  `…\x1b`  |  `[35;1R…`
/// * ESC+bracket: `…\x1b[`  |  `35;1R…`
/// * Partial row: `…\x1b[35;`  |  `1R…`
/// * All-but-R:  `…\x1b[35;1`  |  `R…`
/// * Bare (no ESC at all): `…[35;1R…`  — ConPTY occasionally omits the ESC
pub(crate) fn filter_cpr_chunk(pending: &mut String, chunk: &str) -> String {
    use std::sync::OnceLock;

    // Matches any trailing incomplete CPR prefix so it can be carried forward.
    // A CPR looks like \x1b [ <?> digits ; digits R — every strict prefix of
    // that sequence (that starts with \x1b) is matched here.
    static PARTIAL_RE: OnceLock<regex::Regex> = OnceLock::new();
    let partial_re =
        PARTIAL_RE.get_or_init(|| regex::Regex::new(r"\x1b(?:\[(?:\??\d*(?:;\d*)?)?)?$").unwrap());

    // Matches a complete ESC-prefixed CPR anywhere in the text.
    static FULL_RE: OnceLock<regex::Regex> = OnceLock::new();
    let full_re = FULL_RE.get_or_init(|| regex::Regex::new(r"\x1b\[\??\d+;\d+R").unwrap());

    // Matches a bare CPR that ConPTY emitted without its leading ESC byte.
    // `R` as CSI final byte is reserved exclusively for CPR (terminal→app),
    // so this pattern cannot appear in normal program output.
    static BARE_RE: OnceLock<regex::Regex> = OnceLock::new();
    let bare_re = BARE_RE.get_or_init(|| regex::Regex::new(r"\[\??\d+;\d+R").unwrap());

    // ------------------------------------------------------------------ //
    // 1. Prepend any fragment saved from the previous chunk.              //
    // ------------------------------------------------------------------ //
    let mut combined = std::mem::take(pending);
    combined.push_str(chunk);

    // ------------------------------------------------------------------ //
    // 2. Save any trailing incomplete CPR prefix for the next call.       //
    // ------------------------------------------------------------------ //
    if let Some(m) = partial_re.find(&combined) {
        *pending = m.as_str().to_string();
        combined.truncate(m.start());
    }

    // ------------------------------------------------------------------ //
    // 3. Strip fully-formed ESC-prefixed CPRs.                            //
    // ------------------------------------------------------------------ //
    let combined = full_re.replace_all(&combined, "");

    // ------------------------------------------------------------------ //
    // 4. Strip bare CPRs (ESC missing / dropped by ConPTY).               //
    // ------------------------------------------------------------------ //
    bare_re.replace_all(&combined, "").into_owned()
}

/// Strips only ESC-prefixed CPR device responses (`\x1b[row;colR`) from a
/// chunk. Bare CPR fragments without a leading ESC are intentionally preserved.
#[cfg(test)]
fn strip_device_responses(text: &str) -> String {
    use std::sync::OnceLock;
    static FULL_RE: OnceLock<regex::Regex> = OnceLock::new();
    let full_re = FULL_RE.get_or_init(|| regex::Regex::new(r"\x1b\[\??\d+;\d+R").unwrap());
    full_re.replace_all(text, "").into_owned()
}

/// Returns `true` when `text` contains at least one character that is
/// visually rendered (i.e. not whitespace and not part of an ANSI/VT escape
/// sequence). Used to avoid advancing the silence clock on chunks that
/// contain only cursor-movement or redraw escape sequences.
pub(crate) fn has_visible_content(text: &str) -> bool {
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Skip CSI/OSC/other escape sequences.
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    let mut prev = '\0';
                    for c in chars.by_ref() {
                        if c == '\x07' {
                            break;
                        }
                        if prev == '\x1b' && c == '\\' {
                            break;
                        }
                        prev = c;
                    }
                }
                _ => {
                    chars.next();
                }
            }
        } else if !ch.is_whitespace() && !ch.is_control() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_generate_session_id_is_7_chars() {
        let id = generate_session_id(|_| false);
        assert_eq!(id.len(), 7, "session id must be exactly 7 characters");
    }

    #[test]
    fn test_generate_session_id_is_alphanumeric() {
        let id = generate_session_id(|_| false);
        assert!(
            id.chars().all(|c| c.is_ascii_alphanumeric()),
            "session id must be alphanumeric, got: {id}"
        );
    }

    #[test]
    fn test_generate_session_id_avoids_collision() {
        // Force first two attempts to collide, accept the third.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let call_count = AtomicUsize::new(0);
        let id = generate_session_id(|_| {
            let n = call_count.fetch_add(1, Ordering::Relaxed);
            n < 2
        });
        assert_eq!(id.len(), 7);
        assert!(call_count.load(Ordering::Relaxed) >= 3);
    }

    #[test]
    fn test_generate_session_id_unique_across_many() {
        let mut seen = HashSet::new();
        for _ in 0..200 {
            let id = generate_session_id(|c| seen.contains(c));
            assert!(seen.insert(id.clone()), "duplicate id: {id}");
        }
    }

    // -----------------------------------------------------------------------
    // filter_cpr_chunk — helper
    // -----------------------------------------------------------------------

    /// Run several chunks through `filter_cpr_chunk` with shared pending state,
    /// returning the concatenated filtered output.
    fn feed(chunks: &[&str]) -> String {
        let mut pending = String::new();
        let mut out = String::new();
        for chunk in chunks {
            out.push_str(&filter_cpr_chunk(&mut pending, chunk));
        }
        // Flush whatever is still pending (no more chunks → can't be a valid CPR)
        out.push_str(&pending);
        out
    }

    // -----------------------------------------------------------------------
    // filter_cpr_chunk — CPR stripping
    // -----------------------------------------------------------------------

    #[test]
    fn cpr_full_sequence_stripped() {
        assert_eq!(feed(&["\x1b[3;1R"]), "");
        assert_eq!(feed(&["\x1b[12;1R"]), "");
        assert_eq!(feed(&["\x1b[20;1R"]), "");
        assert_eq!(feed(&["\x1b[35;1R"]), "");
        assert_eq!(feed(&["\x1b[24;80R"]), "");
        assert_eq!(feed(&["\x1b[?3;1R"]), ""); // DEC private variant
    }

    #[test]
    fn cpr_split_after_esc() {
        assert_eq!(feed(&["text\x1b", "[35;1R more"]), "text more");
    }

    #[test]
    fn cpr_split_after_bracket() {
        assert_eq!(feed(&["text\x1b[", "35;1R more"]), "text more");
    }

    #[test]
    fn cpr_split_mid_row() {
        assert_eq!(feed(&["text\x1b[35", ";1R more"]), "text more");
    }

    #[test]
    fn cpr_split_after_semicolon() {
        assert_eq!(feed(&["text\x1b[35;", "1R more"]), "text more");
    }

    #[test]
    fn cpr_split_before_r() {
        assert_eq!(feed(&["text\x1b[35;1", "R more"]), "text more");
    }

    #[test]
    fn cpr_bare_no_esc_global() {
        // ConPTY sometimes emits the CPR with no ESC at all.
        assert_eq!(feed(&["[15;1R"]), "");
        assert_eq!(feed(&["[35;1R"]), "");
        assert_eq!(feed(&["before[35;1Rafter"]), "beforeafter");
        assert_eq!(feed(&["> [12;1R"]), "> ");
    }

    #[test]
    fn cpr_multiple_in_one_chunk() {
        assert_eq!(feed(&["\x1b[3;1R\x1b[24;80R"]), "");
        assert_eq!(feed(&["a\x1b[3;1Rb\x1b[24;80Rc"]), "abc");
    }

    // -----------------------------------------------------------------------
    // filter_cpr_chunk — must NOT mangle normal output
    // -----------------------------------------------------------------------

    #[test]
    fn normal_text_untouched() {
        let plain = "hello world";
        assert_eq!(feed(&[plain]), plain);
    }

    #[test]
    fn fsi_let_binding_untouched() {
        // The exact text that was being corrupted in the regression.
        let line = "let x = 123;;";
        assert_eq!(feed(&[line]), line);
    }

    #[test]
    fn fsi_error_output_untouched() {
        let err = "stdin(1,5): error FS1156: This is not a valid numeric literal.";
        assert_eq!(feed(&[err]), err);
        let caret = "  ----^^^^^";
        assert_eq!(feed(&[caret]), caret);
    }

    #[test]
    fn fsharp_list_output_untouched() {
        // F# list display has `; ` (space after semicolon) — should not match.
        let list = "val it: int list = [1; 2; 3]";
        assert_eq!(feed(&[list]), list);
    }

    #[test]
    fn ansi_sgr_untouched() {
        let sgr = "\x1b[32mOK\x1b[0m";
        assert_eq!(feed(&[sgr]), sgr);
    }

    #[test]
    fn ansi_cursor_movement_untouched() {
        // \x1b[1A = cursor up, \x1b[2K = erase line — neither ends in R.
        let cur = "\x1b[1A\x1b[2K";
        assert_eq!(feed(&[cur]), cur);
    }

    #[test]
    fn cpr_inline_with_normal_output() {
        // CPR injected in the middle of a normal fsi output line.
        assert_eq!(
            feed(&["> let x = \x1b[35;1R123;;\r\n"]),
            "> let x = 123;;\r\n"
        );
    }

    #[test]
    fn cpr_split_across_fsi_output() {
        // CPR split mid-sequence, flanked by genuine fsi output.
        assert_eq!(
            feed(&["> let x = \x1b[35;", "1R123;;\r\n"]),
            "> let x = 123;;\r\n"
        );
    }

    // -----------------------------------------------------------------------
    // strip_device_responses (lower-level, ESC-prefixed only)
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_cpr_simple() {
        assert_eq!(strip_device_responses("\x1b[3;1R"), "");
        assert_eq!(strip_device_responses("\x1b[24;80R"), "");
    }

    #[test]
    fn test_strip_bare_cpr_not_touched() {
        // Bare `[N;NR` (no ESC) is handled by filter_cpr_chunk, not here.
        assert_eq!(strip_device_responses("[12;1R"), "[12;1R");
    }

    #[test]
    fn test_strip_passthrough_normal_ansi() {
        let sgr = "\x1b[32mOK\x1b[0m";
        assert_eq!(strip_device_responses(sgr), sgr);
    }

    // -----------------------------------------------------------------------
    // has_visible_content
    // -----------------------------------------------------------------------

    #[test]
    fn test_has_visible_content_plain_text() {
        assert!(has_visible_content("hello"));
        assert!(has_visible_content("$ "));
        assert!(has_visible_content("Do you want to continue?"));
    }

    #[test]
    fn test_has_visible_content_empty_and_whitespace() {
        assert!(!has_visible_content(""));
        assert!(!has_visible_content("   "));
        assert!(!has_visible_content("\t\n\r"));
    }

    #[test]
    fn test_has_visible_content_pure_ansi_csi() {
        // Cursor movement escape sequences — no visible characters.
        assert!(!has_visible_content("\x1b[2J"));
        assert!(!has_visible_content("\x1b[1A\x1b[1A\x1b[2K"));
        assert!(!has_visible_content("\x1b[H\x1b[2J\x1b[?25l"));
    }

    #[test]
    fn test_has_visible_content_ansi_with_text() {
        // ANSI wrapping around visible text → true.
        assert!(has_visible_content("\x1b[32mOK\x1b[0m"));
        assert!(has_visible_content("\x1b[1;31mError\x1b[0m"));
    }

    #[test]
    fn test_has_visible_content_osc_sequences() {
        // OSC title sequence — no visible characters.
        assert!(!has_visible_content("\x1b]0;my title\x07"));
        assert!(!has_visible_content("\x1b]2;Terminal\x1b\\"));
    }

    // -----------------------------------------------------------------------
    // Helpers for SessionRuntime unit tests
    // -----------------------------------------------------------------------

    struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_writer() -> (
        Box<dyn std::io::Write + Send>,
        std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    ) {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        (Box::new(CaptureWriter(buf.clone())), buf)
    }

    fn make_test_child() -> RuntimeChild {
        #[cfg(target_os = "windows")]
        let mut cmd = portable_pty::CommandBuilder::new("cmd.exe");
        #[cfg(target_os = "windows")]
        cmd.args(["/c", "exit", "0"]);
        #[cfg(not(target_os = "windows"))]
        let mut cmd = portable_pty::CommandBuilder::new("true");

        let pty = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty in test");
        let child = pty.slave.spawn_command(cmd).expect("spawn in test");
        RuntimeChild::Pty(child)
    }

    fn new_runtime(writer: Box<dyn std::io::Write + Send>) -> SessionRuntime {
        use crate::session::{SessionMeta, SessionStatus};
        use std::collections::VecDeque;

        let meta = SessionMeta {
            id: "rt_tst01".to_string(),
            title: None,
            command: "sh".to_string(),
            args: vec![],
            cwd: None,
            created_at: chrono::Utc::now(),
            started_at: Some(chrono::Utc::now()),
            ended_at: None,
            status: SessionStatus::Running,
            pid: None,
            exit_code: None,
        };
        SessionRuntime {
            meta,
            dir: std::env::temp_dir().join("oly_runtime_unit_tests"),
            output_line_count: 0,
            ring: VecDeque::new(),
            ring_limit: 4, // small limit to test eviction
            writer,
            child: make_test_child(),
            completed_at: None,
            _pty_master: None,
            persisted: false,
            last_output_at: None,
            last_input_at: None,
            last_attach_activity_at: None,
            last_notified_at: None,
            notified_output_epoch: None,
            pending_cpr_prefix: String::new(),
            bracketed_paste_mode: false,
            pending_terminal_query_tail: String::new(),
            app_cursor_keys: false,
            notifications_enabled: true,
        }
    }

    // -----------------------------------------------------------------------
    // push_output — mode tracking
    // -----------------------------------------------------------------------

    #[test]
    fn test_push_output_enables_bracketed_paste() {
        let mut rt = new_runtime(Box::new(std::io::sink()));
        assert!(!rt.bracketed_paste_mode);
        rt.push_output("text \x1b[?2004h more".to_string());
        assert!(
            rt.bracketed_paste_mode,
            "bracketed_paste_mode should be set after \\x1b[?2004h"
        );
    }

    #[test]
    fn test_push_output_disables_bracketed_paste() {
        let mut rt = new_runtime(Box::new(std::io::sink()));
        rt.bracketed_paste_mode = true;
        rt.push_output("\x1b[?2004l".to_string());
        assert!(
            !rt.bracketed_paste_mode,
            "bracketed_paste_mode should be cleared after \\x1b[?2004l"
        );
    }

    #[test]
    fn test_push_output_enables_app_cursor_keys() {
        let mut rt = new_runtime(Box::new(std::io::sink()));
        assert!(!rt.app_cursor_keys);
        rt.push_output("\x1b[?1h".to_string());
        assert!(
            rt.app_cursor_keys,
            "app_cursor_keys should be set after DECCKM enable"
        );
    }

    #[test]
    fn test_push_output_disables_app_cursor_keys() {
        let mut rt = new_runtime(Box::new(std::io::sink()));
        rt.app_cursor_keys = true;
        rt.push_output("\x1b[?1l".to_string());
        assert!(
            !rt.app_cursor_keys,
            "app_cursor_keys should be cleared after DECCKM disable"
        );
    }

    // -----------------------------------------------------------------------
    // push_output — ring buffer eviction
    // -----------------------------------------------------------------------

    #[test]
    fn test_push_output_ring_evicts_oldest_when_full() {
        let mut rt = new_runtime(Box::new(std::io::sink()));
        // ring_limit is 4; push 5 lines.
        for i in 0..5usize {
            rt.push_output(format!("line{i}\n"));
        }
        assert_eq!(rt.ring.len(), 4, "ring should not exceed ring_limit");
        // The oldest line ("line0") should be evicted; ring starts at "line1".
        assert_eq!(rt.ring.front().map(String::as_str), Some("line1\n"));
        assert_eq!(rt.ring.back().map(String::as_str), Some("line4\n"));
    }

    #[test]
    fn test_push_output_line_count_tracks_total_lines() {
        let mut rt = new_runtime(Box::new(std::io::sink()));
        for i in 0..10usize {
            rt.push_output(format!("line{i}\n"));
        }
        assert_eq!(
            rt.output_line_count, 10,
            "output_line_count tracks total lines pushed"
        );
        // ring_limit is 4, so ring only holds last 4 lines
        assert_eq!(rt.ring.len(), 4, "ring is bounded by ring_limit");
    }

    // -----------------------------------------------------------------------
    // push_output — last_output_at tracking
    // -----------------------------------------------------------------------

    #[test]
    fn test_push_output_visible_content_advances_last_output_at() {
        let mut rt = new_runtime(Box::new(std::io::sink()));
        assert!(rt.last_output_at.is_none());
        rt.push_output("hello world\n".to_string());
        assert!(
            rt.last_output_at.is_some(),
            "visible output should set last_output_at"
        );
    }

    #[test]
    fn test_push_output_pure_ansi_does_not_advance_last_output_at() {
        let mut rt = new_runtime(Box::new(std::io::sink()));
        // Cursor movement + erase sequences — no visible characters.
        rt.push_output("\x1b[1A\x1b[2K\x1b[H".to_string());
        assert!(
            rt.last_output_at.is_none(),
            "pure ANSI sequences should not advance last_output_at"
        );
    }

    // -----------------------------------------------------------------------
    // mark_attach_activity / has_active_attach_client
    // -----------------------------------------------------------------------

    #[test]
    fn test_has_active_attach_client_false_initially() {
        let rt = new_runtime(Box::new(std::io::sink()));
        assert!(
            !rt.has_active_attach_client(),
            "no activity recorded yet → should report no active client"
        );
    }

    #[test]
    fn test_has_active_attach_client_true_after_mark() {
        let mut rt = new_runtime(Box::new(std::io::sink()));
        rt.mark_attach_activity();
        assert!(
            rt.has_active_attach_client(),
            "should report active client immediately after mark_attach_activity"
        );
    }

    // -----------------------------------------------------------------------
    // respond_to_terminal_queries_without_attach
    // -----------------------------------------------------------------------

    #[test]
    fn test_respond_cpr_query_sends_reply_and_strips_from_output() {
        let (writer, buf) = capture_writer();
        let mut rt = new_runtime(writer);

        // CPR query embedded in a chunk with surrounding text.
        let out = rt.respond_to_terminal_queries_without_attach("before\x1b[6nafter");

        // The sequence itself should be stripped from the returned output.
        assert!(
            !out.contains("\x1b[6n"),
            "CPR query should be stripped from returned output"
        );
        // A CPR response should have been written to the PTY (position 1;1).
        let written = buf.lock().unwrap().clone();
        assert!(
            written.windows(b"\x1b[".len()).any(|w| w == b"\x1b["),
            "a CPR response should be written to the writer"
        );
        assert!(
            std::str::from_utf8(&written)
                .unwrap_or("")
                .contains("\x1b[1;1R"),
            "CPR response should be \\x1b[1;1R"
        );
    }

    #[test]
    fn test_respond_dsr_query_sends_reply_and_strips_from_output() {
        let (writer, buf) = capture_writer();
        let mut rt = new_runtime(writer);

        let out = rt.respond_to_terminal_queries_without_attach("text\x1b[5nmore");

        assert!(
            !out.contains("\x1b[5n"),
            "DSR query should be stripped from returned output"
        );
        let written = buf.lock().unwrap().clone();
        assert_eq!(
            std::str::from_utf8(&written).unwrap_or(""),
            "\x1b[0n",
            "DSR response should be \\x1b[0n"
        );
    }

    #[test]
    fn test_respond_no_query_passes_through_unchanged() {
        let mut rt = new_runtime(Box::new(std::io::sink()));
        let input = "normal output with no queries";
        let out = rt.respond_to_terminal_queries_without_attach(input);
        assert_eq!(
            out, input,
            "chunk without queries should pass through unchanged"
        );
    }

    // -----------------------------------------------------------------------
    // is_completed / mark_completed
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_completed_running_returns_false() {
        let rt = new_runtime(Box::new(std::io::sink()));
        assert!(
            !rt.is_completed(),
            "running session should not be completed"
        );
    }

    #[test]
    fn test_mark_completed_stopped() {
        use crate::session::SessionStatus;
        let mut rt = new_runtime(Box::new(std::io::sink()));
        rt.mark_completed(SessionStatus::Stopped, Some(0));
        assert!(rt.is_completed());
        assert_eq!(rt.meta.exit_code, Some(0));
        assert!(rt.meta.ended_at.is_some());
        assert!(rt.completed_at.is_some());
    }

    #[test]
    fn test_mark_completed_failed_with_nonzero_exit() {
        use crate::session::SessionStatus;
        let mut rt = new_runtime(Box::new(std::io::sink()));
        rt.mark_completed(SessionStatus::Failed, Some(1));
        assert!(rt.is_completed());
        assert_eq!(rt.meta.exit_code, Some(1));
    }

    #[test]
    fn test_mark_completed_is_idempotent() {
        use crate::session::SessionStatus;
        let mut rt = new_runtime(Box::new(std::io::sink()));
        rt.mark_completed(SessionStatus::Stopped, Some(0));
        let first_ended_at = rt.meta.ended_at;
        // Second call should not overwrite ended_at.
        rt.mark_completed(SessionStatus::Stopped, Some(0));
        assert_eq!(
            rt.meta.ended_at, first_ended_at,
            "mark_completed should not overwrite ended_at on second call"
        );
    }
}
