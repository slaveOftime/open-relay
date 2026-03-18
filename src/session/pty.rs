// ---------------------------------------------------------------------------
// PTY-related types and terminal query/escape handling
// ---------------------------------------------------------------------------

use std::sync::OnceLock;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, trace, warn};

// ---------------------------------------------------------------------------
// PtyHandle — owns the pseudo-terminal file descriptors, reader and writer
// threads, and child process
// ---------------------------------------------------------------------------

/// Pure pseudo-terminal ownership struct. Manages the master file descriptor,
/// the child process, and
/// the dedicated reader/writer threads.  No business logic (notifications,
/// session metadata, etc.) lives here.
pub struct PtyHandle {
    pub(crate) child: RuntimeChild,
    /// Channel to the dedicated pseudo-terminal writer thread.
    pub(crate) writer_tx: mpsc::Sender<Vec<u8>>,
    /// Kept alive so the master file descriptor stays open; resize goes through this.
    pub(crate) pty_master: Option<Box<dyn portable_pty::MasterPty + Send>>,
}

impl PtyHandle {
    /// Send raw bytes to the child process standard input via the writer thread.
    pub fn try_write_input(&self, data: Vec<u8>) -> std::result::Result<(), TrySendError<Vec<u8>>> {
        let len = data.len();
        let result = self.writer_tx.try_send(data);
        match &result {
            Ok(()) => trace!(bytes = len, "queued PTY stdin write"),
            Err(TrySendError::Full(_)) => debug!(bytes = len, "PTY stdin queue is full"),
            Err(TrySendError::Closed(_)) => debug!(bytes = len, "PTY stdin queue is closed"),
        }
        result
    }

    /// Resize the pseudo-terminal. Returns `true` on success.
    pub fn resize(&mut self, rows: u16, cols: u16) -> bool {
        let Some(master) = self.pty_master.as_mut() else {
            debug!(
                rows,
                cols, "PTY resize skipped because master handle is unavailable"
            );
            return false;
        };
        let result = master
            .resize(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .is_ok();
        debug!(rows, cols, resized = result, "PTY resize completed");
        result
    }

    /// Release pseudo-terminal-owned handles once the session is completed.
    ///
    /// This closes the stored master handle and replaces the public writer
    /// sender with a permanently closed channel so future writes fail fast.
    pub fn release_resources(&mut self) {
        debug!("releasing PTY resources");
        self.pty_master.take();
        let (closed_tx, closed_rx) = mpsc::channel(1);
        drop(closed_rx);
        let previous_tx = std::mem::replace(&mut self.writer_tx, closed_tx);
        drop(previous_tx);
    }

    /// Forcefully terminate the child process or close the Windows ConPTY handle.
    pub fn kill(&mut self) -> std::io::Result<()> {
        let result = self.child.kill();
        match &result {
            Ok(()) => debug!("PTY child kill requested"),
            Err(err) => warn!(%err, "failed to kill PTY child"),
        }
        result
    }

    /// Perform a non-blocking child-process exit check.
    pub fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        let result = self.child.try_wait_code();
        match &result {
            Ok(Some(code)) => debug!(exit_code = code, "PTY child exit observed"),
            Ok(None) => {}
            Err(err) => warn!(%err, "failed to poll PTY child status"),
        }
        result
    }

    /// Return the child process identifier when the platform exposes one.
    #[allow(dead_code)]
    pub fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }
}

// ---------------------------------------------------------------------------
// RuntimeChild (wraps portable_pty::Child)
// ---------------------------------------------------------------------------

pub enum RuntimeChild {
    Pty(Box<dyn portable_pty::Child + Send + Sync>),
}

impl RuntimeChild {
    /// Return the wrapped child process identifier when available.
    pub fn process_id(&self) -> Option<u32> {
        match self {
            Self::Pty(child) => child.process_id(),
        }
    }

    /// Terminate the wrapped child process.
    pub fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Pty(child) => child.kill(),
        }
    }

    /// Perform a non-blocking wait and normalize the exit status to an `i32`.
    pub fn try_wait_code(&mut self) -> std::io::Result<Option<i32>> {
        match self {
            Self::Pty(child) => child
                .try_wait()
                .map(|opt| opt.map(|status| status.exit_code() as i32)),
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal query types and helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalQuery {
    /// Cursor Position Report query (`CSI 6 n`).
    CursorPositionReport,
    /// Device Status Report query (`CSI 5 n`).
    DeviceStatusReport,
    /// Operating System Command query for the foreground color (`OSC 10`).
    ForegroundColor,
    /// Operating System Command query for the background color (`OSC 11`).
    BackgroundColor,
    /// Primary Device Attributes query (`DA1`).
    PrimaryDeviceAttributes,
    /// Secondary Device Attributes query (`DA2`).
    SecondaryDeviceAttributes,
    /// xterm version query (`XTVERSION`).
    XtVersion,
    /// DEC private mode report query (`DECRPM`) with one or more mode ids.
    DecPrivateModeReport(String),
    /// Kitty keyboard protocol capability query.
    KittyKeyboard,
}

/// Fixed terminal-capability queries that can be matched by exact byte text.
const TERMINAL_QUERY_PATTERNS: [(&str, TerminalQuery); 6] = [
    ("\x1b[6n", TerminalQuery::CursorPositionReport),
    ("\x1b[5n", TerminalQuery::DeviceStatusReport),
    ("\x1b]10;?\x07", TerminalQuery::ForegroundColor),
    ("\x1b]10;?\x1b\\", TerminalQuery::ForegroundColor),
    ("\x1b]11;?\x07", TerminalQuery::BackgroundColor),
    ("\x1b]11;?\x1b\\", TerminalQuery::BackgroundColor),
];

/// Build the string-regular-expression matcher for a trailing partial
/// Control Sequence Introducer sequence.
fn partial_csi_sequence_re() -> &'static regex::Regex {
    static PARTIAL_RE: OnceLock<regex::Regex> = OnceLock::new();
    PARTIAL_RE.get_or_init(|| regex::Regex::new(r"\x1b(?:\[(?:[>?]?\d*(?:;\d*)*\$?)?)?$").unwrap())
}

/// Build the byte-regular-expression matcher for a trailing partial
/// Control Sequence Introducer sequence.
fn partial_csi_sequence_re_bytes() -> &'static regex::bytes::Regex {
    static PARTIAL_RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    PARTIAL_RE
        .get_or_init(|| regex::bytes::Regex::new(r"\x1b(?:\[(?:[>?]?\d*(?:;\d*)*\$?)?)?$").unwrap())
}

/// Build the byte matcher for DEC private Device Status Report queries such
/// as `CSI ? 996 n`.
fn private_dsr_query_re_bytes() -> &'static regex::bytes::Regex {
    static RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::bytes::Regex::new(r"\x1b\[\?\d+n").unwrap())
}

/// Build the string matcher for DEC private mode report queries.
fn decrpm_query_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\x1b\[\?(\d+(?:;\d+)*)\$p").unwrap())
}

/// Build the byte matcher for DEC private mode report queries.
fn decrpm_query_re_bytes() -> &'static regex::bytes::Regex {
    static RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::bytes::Regex::new(r"\x1b\[\?(\d+(?:;\d+)*)\$p").unwrap())
}

/// Build the string matcher for xterm version queries.
fn xtversion_query_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\x1b\[>\d*q").unwrap())
}

/// Build the byte matcher for xterm version queries.
fn xtversion_query_re_bytes() -> &'static regex::bytes::Regex {
    static RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::bytes::Regex::new(r"\x1b\[>\d*q").unwrap())
}

/// Build the string matcher for primary Device Attributes queries.
fn da1_query_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\x1b\[\d*c").unwrap())
}

/// Build the byte matcher for primary Device Attributes queries.
fn da1_query_re_bytes() -> &'static regex::bytes::Regex {
    static RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::bytes::Regex::new(r"\x1b\[\d*c").unwrap())
}

/// Build the string matcher for secondary Device Attributes queries.
fn da2_query_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\x1b\[>\d*c").unwrap())
}

/// Build the byte matcher for secondary Device Attributes queries.
fn da2_query_re_bytes() -> &'static regex::bytes::Regex {
    static RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::bytes::Regex::new(r"\x1b\[>\d*c").unwrap())
}

/// Build the string matcher for Kitty keyboard capability queries.
fn kitty_keyboard_query_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\x1b\[\?u").unwrap())
}

/// Build the byte matcher for Kitty keyboard capability queries.
fn kitty_keyboard_query_re_bytes() -> &'static regex::bytes::Regex {
    static RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::bytes::Regex::new(r"\x1b\[\?u").unwrap())
}

/// Find the earliest terminal-capability query in `text` starting at
/// `search_from` and classify it into a typed query variant.
pub fn find_next_terminal_query(
    text: &str,
    search_from: usize,
) -> Option<(usize, usize, TerminalQuery)> {
    let fixed_match = TERMINAL_QUERY_PATTERNS
        .iter()
        .filter_map(|(pattern, query)| {
            text[search_from..]
                .find(pattern)
                .map(|offset| (search_from + offset, pattern.len(), query.clone()))
        })
        .min_by_key(|(start, _, _)| *start);

    let decrpm_match = decrpm_query_re()
        .captures_at(text, search_from)
        .and_then(|caps| {
            let whole = caps.get(0)?;
            let modes = caps.get(1)?.as_str().to_string();
            Some((
                whole.start(),
                whole.as_str().len(),
                TerminalQuery::DecPrivateModeReport(modes),
            ))
        });

    let xtversion_match = xtversion_query_re()
        .find_at(text, search_from)
        .map(|m| (m.start(), m.as_str().len(), TerminalQuery::XtVersion));

    let da1_match = da1_query_re().find_at(text, search_from).map(|m| {
        (
            m.start(),
            m.as_str().len(),
            TerminalQuery::PrimaryDeviceAttributes,
        )
    });

    let da2_match = da2_query_re().find_at(text, search_from).map(|m| {
        (
            m.start(),
            m.as_str().len(),
            TerminalQuery::SecondaryDeviceAttributes,
        )
    });

    let kitty_match = kitty_keyboard_query_re()
        .find_at(text, search_from)
        .map(|m| (m.start(), m.as_str().len(), TerminalQuery::KittyKeyboard));

    [
        fixed_match,
        decrpm_match,
        xtversion_match,
        da1_match,
        da2_match,
        kitty_match,
    ]
    .into_iter()
    .flatten()
    .min_by_key(|(start, _, _)| *start)
}

/// Return how many bytes at the end of `remainder` must be retained because
/// they form an incomplete terminal-capability query.
pub fn terminal_query_tail_len(remainder: &str) -> usize {
    let csi_tail = partial_csi_sequence_re()
        .find(remainder)
        .filter(|m| m.end() == remainder.len())
        .map(|m| remainder.len().saturating_sub(m.start()))
        .unwrap_or(0);

    let osc_tail = [
        "\x1b]10;?\x1b",
        "\x1b]11;?\x1b",
        "\x1b]10;?",
        "\x1b]11;?",
        "\x1b]10;",
        "\x1b]11;",
        "\x1b]",
    ]
    .into_iter()
    .filter_map(|prefix| remainder.rfind(prefix).map(|start| (prefix, start)))
    .filter(|(_, start)| {
        let suffix = &remainder[*start..];
        !suffix.contains('\x07') && !suffix.contains("\x1b\\")
    })
    .map(|(_, start)| remainder.len().saturating_sub(start))
    .max()
    .unwrap_or(0);

    csi_tail.max(osc_tail)
}

/// Format the daemon's synthetic reply to an xterm version query.
fn xtversion_response() -> String {
    format!(
        "\x1bP>|{} {}\x1b\\",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    )
}

/// Format one DEC private mode report response per queried mode id.
fn decrpm_responses(modes: &str) -> Vec<String> {
    modes
        .split(';')
        .filter(|mode| !mode.is_empty())
        .map(|mode| format!("\x1b[?{mode};2$y"))
        .collect()
}

/// Convert a parsed terminal-capability query into one or more response
/// frames that can be written back to the pseudo terminal.
pub fn terminal_query_response(query: TerminalQuery, cursor: Option<(u16, u16)>) -> Vec<String> {
    match query {
        TerminalQuery::CursorPositionReport => {
            let (row, col) = cursor.unwrap_or((1, 1));
            vec![format!("\x1b[{row};{col}R")]
        }
        TerminalQuery::DeviceStatusReport => vec!["\x1b[0n".to_string()],
        TerminalQuery::ForegroundColor => {
            let (foreground, _) = terminal_report_colors();
            vec![format_osc_color_response(10, &foreground)]
        }
        TerminalQuery::BackgroundColor => {
            let (_, background) = terminal_report_colors();
            vec![format_osc_color_response(11, &background)]
        }
        TerminalQuery::PrimaryDeviceAttributes => vec!["\x1b[?62;c".to_string()],
        TerminalQuery::SecondaryDeviceAttributes => vec!["\x1b[>1;0;0c".to_string()],
        TerminalQuery::XtVersion => vec![xtversion_response()],
        TerminalQuery::DecPrivateModeReport(modes) => decrpm_responses(&modes),
        TerminalQuery::KittyKeyboard => vec!["\x1b[?0u".to_string()],
    }
}

/// Derive foreground and background Operating System Command color responses
/// from the environment, falling back to a conservative white-on-black pair.
fn terminal_report_colors() -> (String, String) {
    if let Ok(raw) = std::env::var("COLORFGBG") {
        let parsed: Vec<u8> = raw
            .split(';')
            .filter_map(|part| part.trim().parse::<u8>().ok())
            .collect();
        if parsed.len() >= 2 {
            let foreground = xterm_color_to_rgb(parsed[parsed.len() - 2]);
            let background = xterm_color_to_rgb(parsed[parsed.len() - 1]);
            return (format_osc_rgb(foreground), format_osc_rgb(background));
        }
    }

    (
        "rgb:ffff/ffff/ffff".to_string(),
        "rgb:0000/0000/0000".to_string(),
    )
}

/// Format one Operating System Command color response.
fn format_osc_color_response(ps: u8, color: &str) -> String {
    format!("\x1b]{ps};{color}\x1b\\")
}

/// Convert an RGB tuple into xterm's `rgb:rrrr/gggg/bbbb` string format.
fn format_osc_rgb((red, green, blue): (u8, u8, u8)) -> String {
    format!("rgb:{red:02x}{red:02x}/{green:02x}{green:02x}/{blue:02x}{blue:02x}")
}

/// Convert an xterm 256-color palette index into an RGB triple.
fn xterm_color_to_rgb(index: u8) -> (u8, u8, u8) {
    match index {
        0 => (0x00, 0x00, 0x00),
        1 => (0xcd, 0x00, 0x00),
        2 => (0x00, 0xcd, 0x00),
        3 => (0xcd, 0xcd, 0x00),
        4 => (0x00, 0x00, 0xee),
        5 => (0xcd, 0x00, 0xcd),
        6 => (0x00, 0xcd, 0xcd),
        7 => (0xe5, 0xe5, 0xe5),
        8 => (0x7f, 0x7f, 0x7f),
        9 => (0xff, 0x00, 0x00),
        10 => (0x00, 0xff, 0x00),
        11 => (0xff, 0xff, 0x00),
        12 => (0x5c, 0x5c, 0xff),
        13 => (0xff, 0x00, 0xff),
        14 => (0x00, 0xff, 0xff),
        15 => (0xff, 0xff, 0xff),
        16..=231 => {
            let value = index - 16;
            let red = value / 36;
            let green = (value % 36) / 6;
            let blue = value % 6;
            let levels = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
            (
                levels[red as usize],
                levels[green as usize],
                levels[blue as usize],
            )
        }
        232..=255 => {
            let level = 8 + (index - 232) * 10;
            (level, level, level)
        }
    }
}

// ---------------------------------------------------------------------------
// EscapeFilter — stateful per-attach ESC sequence stripper
// ---------------------------------------------------------------------------

/// Strips Cursor Position Report and Device Status Report responses, plus
/// Operating System Command color responses, from pseudo-terminal output
/// before display. Carries incomplete sequences across chunk boundaries so
/// Windows ConPTY split-escape cases are handled correctly.
///
/// All regex state is static (shared); only the cross-chunk `pending` prefix
/// is per-instance.
pub struct EscapeFilter {
    pending: Vec<u8>,
}

impl EscapeFilter {
    /// Create an empty byte-preserving escape filter.
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Filter one raw pseudo-terminal byte slice and return the cleaned bytes.
    pub fn filter(&mut self, data: &[u8]) -> Vec<u8> {
        let pending_before = self.pending.len();
        let filtered = filter_cpr_chunk_bytes(&mut self.pending, data);
        if filtered.len() != data.len() || self.pending.len() != pending_before {
            trace!(
                input_bytes = data.len(),
                output_bytes = filtered.len(),
                pending_before,
                pending_after = self.pending.len(),
                "filtered terminal escape responses from PTY output"
            );
        }
        filtered
    }
}

impl Default for EscapeFilter {
    /// Create the default byte-preserving escape filter.
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Daemon-side fallback query responder (no attach client)
// ---------------------------------------------------------------------------

/// Scan `data` for terminal-capability queries and generate response bytes.
///
/// Responses are for the daemon to write back to the PTY stdin when no attach
/// client is connected. Cursor Position Report replies always use row 1 col 1
/// when no live terminal is attached. `tail` carries partial query sequences
/// across chunk boundaries.
///
/// Returns a list of response byte strings to write sequentially.
///
/// Operating System Command color probes are answered with a static best-guess
/// response (white foreground, black background, or the `COLORFGBG` env var if
/// set). Primary Device Attributes, Secondary Device Attributes, xterm version,
/// DEC private mode report, and Kitty keyboard probes get conservative,
/// well-formed replies so detached apps do not block waiting for a real
/// terminal to answer them.
pub fn extract_query_responses_no_client(
    data: &[u8],
    tail: &mut String,
    cursor: (u16, u16),
) -> Vec<Vec<u8>> {
    let text = String::from_utf8_lossy(data);
    let mut combined = std::mem::take(tail);
    combined.push_str(&text);

    let mut responses = Vec::new();
    let mut search_from = 0usize;

    while search_from < combined.len() {
        let Some((match_start, query_len, query)) =
            find_next_terminal_query(&combined, search_from)
        else {
            break;
        };
        let response_cursor = match query {
            TerminalQuery::CursorPositionReport | TerminalQuery::DeviceStatusReport => Some(cursor),
            _ => None,
        };
        for response in terminal_query_response(query, response_cursor) {
            responses.push(response.into_bytes());
        }
        search_from = match_start + query_len;
    }

    let remainder = &combined[search_from..];
    let keep = terminal_query_tail_len(remainder);
    *tail = remainder[remainder.len().saturating_sub(keep)..].to_string();

    if !responses.is_empty() || keep > 0 {
        debug!(
            input_bytes = data.len(),
            response_count = responses.len(),
            tail_len = keep,
            cursor_row = cursor.0,
            cursor_col = cursor.1,
            "processed terminal capability queries without a live client"
        );
    }

    responses
}

// ---------------------------------------------------------------------------
// filter_cpr_chunk — shared terminal-response filter (used by EscapeFilter)
// ---------------------------------------------------------------------------

/// Filter one pseudo-terminal chunk and strip synthetic terminal-response
/// traffic such as Cursor Position Report replies, Device Status Report
/// replies, Operating System Command color replies, and terminal-capability
/// queries that would make the local terminal answer on its own.
///
/// Windows ConPTY can echo device and color responses into the master output
/// stream. The sequence is frequently split across read boundaries at any
/// byte, so `pending` carries a trailing prefix from one call to the next to
/// ensure every fragment is reassembled before being examined.
///
/// Splitting points handled (all stripped):
/// * Full Cursor Position Report in one chunk: `\x1b[35;1R`
/// * ESC alone:               `…\x1b`  |  `[35;1R…`
/// * Bare form with no Escape prefix from ConPTY: `…[35;1R…`
/// * Full or bare Operating System Command 10 and 11 variants
/// * Terminal queries that would cause the attach client's terminal to
///   respond (DEC private mode report, xterm version, primary and secondary
///   Device Attributes, Kitty keyboard, and related queries)
fn filter_cpr_chunk_bytes(pending: &mut Vec<u8>, chunk: &[u8]) -> Vec<u8> {
    let pending_before = pending.len();
    static FULL_RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let full_re = FULL_RE.get_or_init(|| regex::bytes::Regex::new(r"\x1b\[\??\d+;\d+R").unwrap());

    static BARE_RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let bare_re = BARE_RE.get_or_init(|| regex::bytes::Regex::new(r"\[\??\d+;\d+R").unwrap());

    static DSR_QUERY_FULL_RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let dsr_query_full_re =
        DSR_QUERY_FULL_RE.get_or_init(|| regex::bytes::Regex::new(r"\x1b\[\d+n").unwrap());

    static DSR_QUERY_BARE_RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let dsr_query_bare_re =
        DSR_QUERY_BARE_RE.get_or_init(|| regex::bytes::Regex::new(r"\[[56]n").unwrap());

    static WINSIZE_QUERY_RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let winsize_query_re =
        WINSIZE_QUERY_RE.get_or_init(|| regex::bytes::Regex::new(r"\x1b\[1[4-9]t").unwrap());

    static OSC_FULL_RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let osc_full_re = OSC_FULL_RE.get_or_init(|| {
        regex::bytes::Regex::new(
            r"\x1b]1(?:0|1);rgb:[0-9A-Fa-f]{4}/[0-9A-Fa-f]{4}/[0-9A-Fa-f]{4}(?:\x07|\x1b\\)",
        )
        .unwrap()
    });

    static OSC_BARE_RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let osc_bare_re = OSC_BARE_RE.get_or_init(|| {
        regex::bytes::Regex::new(
            r"]1(?:0|1);rgb:[0-9A-Fa-f]{4}/[0-9A-Fa-f]{4}/[0-9A-Fa-f]{4}(?:\x07|\\)",
        )
        .unwrap()
    });

    static GENERIC_OSC_FULL_RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let generic_osc_full_re = GENERIC_OSC_FULL_RE.get_or_init(|| {
        regex::bytes::Regex::new(r"\x1b]\d{1,3}(?:;[^\x07\x1b]*)*(?:\x07|\x1b\\)").unwrap()
    });

    static GENERIC_OSC_BARE_RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let generic_osc_bare_re = GENERIC_OSC_BARE_RE
        .get_or_init(|| regex::bytes::Regex::new(r"]\d{1,3}(?:;[^\x07\\]*)*(?:\x07|\\)").unwrap());

    static APC_FULL_RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let apc_full_re =
        APC_FULL_RE.get_or_init(|| regex::bytes::Regex::new(r"\x1b_[^\x1b]*\x1b\\").unwrap());

    static DCS_FULL_RE: OnceLock<regex::bytes::Regex> = OnceLock::new();
    let dcs_full_re =
        DCS_FULL_RE.get_or_init(|| regex::bytes::Regex::new(r"\x1bP[^\x1b]*\x1b\\").unwrap());

    let mut combined = std::mem::take(pending);
    combined.extend_from_slice(chunk);

    let cpr_pending_start = partial_csi_sequence_re_bytes()
        .find(&combined)
        .map(|m| m.start());
    let osc_pending_start = trailing_partial_osc_sequence_start_bytes(&combined);
    let apc_dcs_pending_start = trailing_partial_apc_dcs_start_bytes(&combined);
    if let Some(start) = [cpr_pending_start, osc_pending_start, apc_dcs_pending_start]
        .into_iter()
        .flatten()
        .min()
    {
        *pending = combined[start..].to_vec();
        combined.truncate(start);
    }

    let combined = full_re.replace_all(&combined, b"" as &[u8]);
    let combined = bare_re.replace_all(&combined, b"" as &[u8]);
    let combined = dsr_query_full_re.replace_all(&combined, b"" as &[u8]);
    let combined = dsr_query_bare_re.replace_all(&combined, b"" as &[u8]);
    let combined = osc_full_re.replace_all(&combined, b"" as &[u8]);
    let combined = osc_bare_re.replace_all(&combined, b"" as &[u8]);
    let combined = generic_osc_full_re.replace_all(&combined, b"" as &[u8]);
    let combined = generic_osc_bare_re.replace_all(&combined, b"" as &[u8]);
    let combined = private_dsr_query_re_bytes().replace_all(&combined, b"" as &[u8]);
    let combined = decrpm_query_re_bytes().replace_all(&combined, b"" as &[u8]);
    let combined = xtversion_query_re_bytes().replace_all(&combined, b"" as &[u8]);
    let combined = da1_query_re_bytes().replace_all(&combined, b"" as &[u8]);
    let combined = da2_query_re_bytes().replace_all(&combined, b"" as &[u8]);
    let combined = kitty_keyboard_query_re_bytes().replace_all(&combined, b"" as &[u8]);
    let combined = winsize_query_re.replace_all(&combined, b"" as &[u8]);
    let combined = apc_full_re.replace_all(&combined, b"" as &[u8]);

    let filtered = dcs_full_re
        .replace_all(&combined, b"" as &[u8])
        .into_owned();
    if filtered.len() != chunk.len() + pending_before || pending.len() != pending_before {
        trace!(
            pending_before,
            pending_after = pending.len(),
            combined_len = chunk.len() + pending_before,
            filtered_len = filtered.len(),
            "stripped device-response escape sequences from PTY chunk"
        );
    }
    filtered
}

/// Detect a trailing partial Application Program Command or Device Control
/// String sequence that has not yet been terminated by a String Terminator.
fn trailing_partial_apc_dcs_start_bytes(text: &[u8]) -> Option<usize> {
    [b"\x1b_".as_slice(), b"\x1bP".as_slice()]
        .into_iter()
        .filter_map(|prefix| rfind_bytes(text, prefix))
        .filter(|&start| {
            !text[start + 2..]
                .windows(2)
                .any(|window| window == b"\x1b\\")
        })
        .min()
}

/// Detect the earliest trailing partial Operating System Command sequence that
/// should be carried into the next chunk.
fn trailing_partial_osc_sequence_start_bytes(text: &[u8]) -> Option<usize> {
    [
        b"\x1b]10;rgb:".as_slice(),
        b"\x1b]11;rgb:".as_slice(),
        b"]10;rgb:".as_slice(),
        b"]11;rgb:".as_slice(),
    ]
    .into_iter()
    .filter_map(|prefix| rfind_bytes(text, prefix))
    .filter(|start| is_partial_osc_color_response_bytes(&text[*start..]))
    .chain(
        [b"\x1b]".as_slice(), b"]".as_slice()]
            .into_iter()
            .filter_map(|prefix| rfind_bytes(text, prefix))
            .filter(|start| is_partial_generic_osc_sequence_bytes(&text[*start..])),
    )
    .min()
}

/// Return `true` when `candidate` is an incomplete Operating System Command
/// 10 or 11 color response prefix that must stay buffered.
fn is_partial_osc_color_response_bytes(candidate: &[u8]) -> bool {
    let mut index = 0usize;

    if candidate.first() == Some(&0x1b) {
        index += 1;
    }
    if candidate.get(index) != Some(&b']') {
        return false;
    }
    index += 1;

    let rest = &candidate[index..];
    let prefix_len = if rest.starts_with(b"10;rgb:") || rest.starts_with(b"11;rgb:") {
        7
    } else {
        return false;
    };
    index += prefix_len;

    let mut slash_count = 0usize;
    let mut hex_in_component = 0usize;
    let mut saw_hex = false;

    while let Some(&byte) = candidate.get(index) {
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
            0x07 => return false,
            0x1b => {
                return if index + 1 == candidate.len() {
                    slash_count == 2 && saw_hex
                } else {
                    candidate.get(index + 1) != Some(&b'\\')
                };
            }
            b'\\' => return false,
            _ => return false,
        }
    }
    true
}

/// Return `true` when `candidate` is an incomplete generic Operating System
/// Command sequence that must stay buffered.
fn is_partial_generic_osc_sequence_bytes(candidate: &[u8]) -> bool {
    let mut index = 0usize;

    if candidate.first() == Some(&0x1b) {
        index += 1;
    }
    if candidate.get(index) != Some(&b']') {
        return false;
    }
    index += 1;

    let digits_start = index;
    while let Some(&byte) = candidate.get(index) {
        if byte.is_ascii_digit() {
            index += 1;
            continue;
        }
        break;
    }

    if index == digits_start || index - digits_start > 3 {
        return false;
    }
    if candidate.get(index) != Some(&b';') {
        return false;
    }
    index += 1;

    while let Some(&byte) = candidate.get(index) {
        match byte {
            0x07 => return false,
            0x1b => {
                return if index + 1 == candidate.len() {
                    true
                } else {
                    candidate.get(index + 1) != Some(&b'\\')
                };
            }
            b'\\' => return false,
            _ => index += 1,
        }
    }

    true
}

/// Find the last occurrence of `needle` inside `haystack`.
fn rfind_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(haystack.len());
    }
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

// ---------------------------------------------------------------------------
// has_visible_content
// ---------------------------------------------------------------------------

/// Return `true` when `text` contains at least one visibly rendered character,
/// rather than only whitespace or ANSI or VT control sequences.
///
/// This is used to avoid advancing the silence clock on chunks that contain
/// only cursor movement, redraw operations, or other control traffic.
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
                // OSC (\x1b]), APC (\x1b_), DCS (\x1bP), PM (\x1b^)
                // All terminated by ST (\x1b\\) or BEL (\x07).
                Some(']') | Some('_') | Some('P') | Some('^') => {
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

/// Filter terminal-response traffic from replay chunks and concatenate the
/// cleaned bytes into one buffer for attach initialization.
///
/// This is used by both the inter-process communication and WebSocket attach
/// handlers.
pub fn collect_filtered_chunks(
    chunks: &[(u64, bytes::Bytes)],
    filter: &mut EscapeFilter,
) -> Vec<u8> {
    let mut filtered = Vec::with_capacity(chunks.iter().map(|(_, chunk)| chunk.len()).sum());
    for (_, chunk) in chunks {
        filtered.extend(filter.filter(chunk));
    }
    filtered
}

#[cfg(test)]
mod tests {
    use super::*;

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
    // Terminal query / escape filter tests (from utils.rs)
    // -----------------------------------------------------------------------

    #[test]
    fn test_find_next_terminal_query_matches_osc_before_csi() {
        let text = "before\x1b]10;?\x07middle\x1b[6nafter";
        let found = find_next_terminal_query(text, 0);
        assert_eq!(
            found,
            Some((6, "\x1b]10;?\x07".len(), TerminalQuery::ForegroundColor))
        );
    }

    #[test]
    fn test_terminal_query_tail_len_keeps_partial_osc_sequence() {
        assert_eq!(
            terminal_query_tail_len("text\x1b]10;?\x1b"),
            "\x1b]10;?\x1b".len()
        );
    }

    #[test]
    fn test_terminal_query_tail_len_keeps_partial_decrpm_sequence() {
        assert_eq!(
            terminal_query_tail_len("text\x1b[?2004$"),
            "\x1b[?2004$".len()
        );
    }

    #[test]
    fn test_terminal_query_response_formats_osc_colors() {
        assert_eq!(
            format_osc_color_response(10, "rgb:ffff/ffff/ffff"),
            "\x1b]10;rgb:ffff/ffff/ffff\x1b\\"
        );
        assert_eq!(
            format_osc_color_response(11, "rgb:0000/0000/0000"),
            "\x1b]11;rgb:0000/0000/0000\x1b\\"
        );
    }

    #[test]
    fn test_terminal_query_response_formats_capability_defaults() {
        assert_eq!(
            terminal_query_response(TerminalQuery::PrimaryDeviceAttributes, None),
            vec!["\x1b[?62;c".to_string()]
        );
        assert_eq!(
            terminal_query_response(TerminalQuery::SecondaryDeviceAttributes, None),
            vec!["\x1b[>1;0;0c".to_string()]
        );
        assert_eq!(
            terminal_query_response(TerminalQuery::KittyKeyboard, None),
            vec!["\x1b[?0u".to_string()]
        );
        assert_eq!(
            terminal_query_response(
                TerminalQuery::DecPrivateModeReport("2004".to_string()),
                None
            ),
            vec!["\x1b[?2004;2$y".to_string()]
        );
        assert_eq!(
            terminal_query_response(TerminalQuery::XtVersion, None),
            vec![format!(
                "\x1bP>|{} {}\x1b\\",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            )]
        );
    }

    #[test]
    fn test_xterm_color_to_rgb_cube_and_grayscale() {
        assert_eq!(xterm_color_to_rgb(16), (0x00, 0x00, 0x00));
        assert_eq!(xterm_color_to_rgb(21), (0x00, 0x00, 0xff));
        assert_eq!(xterm_color_to_rgb(232), (0x08, 0x08, 0x08));
        assert_eq!(xterm_color_to_rgb(255), (0xee, 0xee, 0xee));
    }

    fn filter_text_chunk(pending: &mut Vec<u8>, chunk: &str) -> String {
        String::from_utf8(filter_cpr_chunk_bytes(pending, chunk.as_bytes()))
            .expect("test chunk should remain valid UTF-8 after filtering")
    }

    fn pending_text(pending: &[u8]) -> &str {
        std::str::from_utf8(pending).expect("test pending bytes should remain valid UTF-8")
    }

    #[test]
    fn test_filter_cpr_chunk_strips_generic_osc_sequences() {
        let mut pending = Vec::new();
        let text = "before\x1b]7;file://host/home/binwen/open-relay/target/debug\x07after";
        assert_eq!(filter_text_chunk(&mut pending, text), "beforeafter");
        assert!(pending.is_empty());
    }

    #[test]
    fn test_filter_cpr_chunk_keeps_partial_generic_osc_sequence_pending() {
        let mut pending = Vec::new();
        let text = "before\x1b]7;file://host/home/binwen/open-relay/target/debug";
        assert_eq!(filter_text_chunk(&mut pending, text), "before");
        assert_eq!(
            pending_text(&pending),
            "\x1b]7;file://host/home/binwen/open-relay/target/debug"
        );
    }

    #[test]
    fn test_filter_cpr_chunk_strips_dsr_queries() {
        let mut pending = Vec::new();
        // CPR query \x1b[6n and DSR query \x1b[5n should be stripped.
        let text = "hello\x1b[6nworld\x1b[5n!";
        assert_eq!(filter_text_chunk(&mut pending, text), "helloworld!");
        assert!(pending.is_empty());
    }

    #[test]
    fn test_filter_cpr_chunk_strips_split_dsr_query() {
        let mut pending = Vec::new();
        // First chunk ends with partial sequence \x1b[6
        let text1 = "hello\x1b[6";
        assert_eq!(filter_text_chunk(&mut pending, text1), "hello");
        // Second chunk completes the query with `n`
        let text2 = "nworld";
        assert_eq!(filter_text_chunk(&mut pending, text2), "world");
    }

    // -----------------------------------------------------------------------
    // Terminal query stripping (DECRPM, XTVERSION, DA, kitty keyboard, etc.)
    // -----------------------------------------------------------------------

    #[test]
    fn test_filter_strips_decrpm_queries() {
        let mut pending = Vec::new();
        let text = "before\x1b[?2004$pafter";
        assert_eq!(filter_text_chunk(&mut pending, text), "beforeafter");
    }

    #[test]
    fn test_filter_strips_multiple_decrpm_queries() {
        let mut pending = Vec::new();
        let text = "\x1b[?1016$p\x1b[?2027$p\x1b[?2004$pvisible";
        assert_eq!(filter_text_chunk(&mut pending, text), "visible");
    }

    #[test]
    fn test_filter_strips_xtversion_query() {
        let mut pending = Vec::new();
        let text = "before\x1b[>0qafter";
        assert_eq!(filter_text_chunk(&mut pending, text), "beforeafter");
    }

    #[test]
    fn test_filter_strips_da1_query() {
        let mut pending = Vec::new();
        let text = "before\x1b[cafter";
        assert_eq!(filter_text_chunk(&mut pending, text), "beforeafter");
    }

    #[test]
    fn test_filter_strips_da2_query() {
        let mut pending = Vec::new();
        let text = "before\x1b[>cafter";
        assert_eq!(filter_text_chunk(&mut pending, text), "beforeafter");
    }

    #[test]
    fn test_filter_strips_kitty_keyboard_query() {
        let mut pending = Vec::new();
        let text = "before\x1b[?uafter";
        assert_eq!(filter_text_chunk(&mut pending, text), "beforeafter");
    }

    #[test]
    fn test_filter_strips_private_dsr_query() {
        let mut pending = Vec::new();
        // \x1b[?996n is a private-mode DSR that the plain \x1b[\d+n regex misses.
        let text = "before\x1b[?996nafter";
        assert_eq!(filter_text_chunk(&mut pending, text), "beforeafter");
    }

    #[test]
    fn test_filter_strips_winsize_query() {
        let mut pending = Vec::new();
        let text = "before\x1b[14tafter";
        assert_eq!(filter_text_chunk(&mut pending, text), "beforeafter");
    }

    #[test]
    fn test_filter_preserves_restore_cursor_csi_u() {
        let mut pending = Vec::new();
        // \x1b[u is "restore cursor position" — must NOT be stripped.
        // Only \x1b[?u (kitty keyboard query) should be stripped.
        let text = "before\x1b[uafter";
        assert_eq!(filter_text_chunk(&mut pending, text), "before\x1b[uafter");
    }

    #[test]
    fn test_filter_strips_opencode_startup_queries() {
        let mut pending = Vec::new();
        // Simulates the query burst that opencode/bubbletea sends at startup.
        let text = "\x1b[>0q\x1b[?25l\x1b[s\x1b[?1016$p\x1b[?2027$p\x1b[?2031$p\x1b[?1004$p\x1b[?2004$p\x1b[?2026$p\x1b[?u\x1b[H\x1b[?1049hTUI_CONTENT";
        let result = filter_text_chunk(&mut pending, text);
        // Queries stripped, mode-setting commands and content preserved.
        assert!(
            result.contains("\x1b[?25l"),
            "hide cursor should be preserved"
        );
        assert!(
            result.contains("\x1b[?1049h"),
            "alt screen enter should be preserved"
        );
        assert!(
            result.contains("TUI_CONTENT"),
            "TUI content should be preserved"
        );
        assert!(!result.contains("$p"), "DECRPM queries should be stripped");
        assert!(!result.contains(">0q"), "XTVERSION should be stripped");
        assert!(
            !result.contains("\x1b[?u"),
            "kitty kb query should be stripped"
        );
    }

    #[test]
    fn test_filter_strips_split_decrpm_query() {
        let mut pending = Vec::new();
        // First chunk ends with partial DECRPM: \x1b[?2004$
        let text1 = "hello\x1b[?2004$";
        assert_eq!(filter_text_chunk(&mut pending, text1), "hello");
        // Second chunk completes the query with `p`
        let text2 = "pworld";
        assert_eq!(filter_text_chunk(&mut pending, text2), "world");
    }

    #[test]
    fn test_extract_query_uses_cursor_position() {
        let mut tail = String::new();
        let data = b"\x1b[6n";
        let responses = extract_query_responses_no_client(data, &mut tail, (7, 3));
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0], b"\x1b[7;3R");
    }

    #[test]
    fn test_escape_filter_preserves_invalid_utf8_bytes() {
        let mut filter = EscapeFilter::new();
        let input = b"before\x80\x1b[6nafter\xff";
        assert_eq!(filter.filter(input), b"before\x80after\xff");
    }

    #[test]
    fn test_escape_filter_preserves_invalid_utf8_bytes_across_split_query() {
        let mut filter = EscapeFilter::new();
        assert_eq!(filter.filter(b"before\x80\x1b[6"), b"before\x80");
        assert_eq!(filter.filter(b"nafter\xff"), b"after\xff");
    }

    #[test]
    fn test_extract_query_answers_device_attributes_and_xtversion() {
        let mut tail = String::new();
        let data = b"\x1b[c\x1b[>c\x1b[>0q";
        let responses = extract_query_responses_no_client(data, &mut tail, (1, 1));
        assert_eq!(responses.len(), 3);
        assert_eq!(responses[0], b"\x1b[?62;c");
        assert_eq!(responses[1], b"\x1b[>1;0;0c");
        assert_eq!(
            responses[2],
            format!(
                "\x1bP>|{} {}\x1b\\",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            )
            .into_bytes()
        );
    }

    #[test]
    fn test_extract_query_answers_split_da2_query() {
        let mut tail = String::new();
        let responses1 = extract_query_responses_no_client(b"hello\x1b[>", &mut tail, (1, 1));
        assert!(responses1.is_empty());
        assert_eq!(tail, "\x1b[>");

        let responses2 = extract_query_responses_no_client(b"cworld", &mut tail, (1, 1));
        assert_eq!(responses2, vec![b"\x1b[>1;0;0c".to_vec()]);
        assert!(tail.is_empty());
    }

    #[test]
    fn test_extract_query_answers_decrpm_and_kitty_keyboard_queries() {
        let mut tail = String::new();
        let data = b"\x1b[?2004$p\x1b[?u";
        let responses = extract_query_responses_no_client(data, &mut tail, (1, 1));
        assert_eq!(
            responses,
            vec![b"\x1b[?2004;2$y".to_vec(), b"\x1b[?0u".to_vec()]
        );
    }

    // -----------------------------------------------------------------------
    // APC / DCS / Kitty graphics filtering
    // -----------------------------------------------------------------------

    #[test]
    fn test_filter_strips_kitty_graphics_apc_response() {
        let mut pending = Vec::new();
        let text = "before\x1b_Gi=31337;OK\x1b\\after";
        assert_eq!(filter_text_chunk(&mut pending, text), "beforeafter");
        assert!(pending.is_empty());
    }

    #[test]
    fn test_filter_strips_kitty_graphics_apc_query() {
        let mut pending = Vec::new();
        let text = "before\x1b_Gi=31337,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\after";
        assert_eq!(filter_text_chunk(&mut pending, text), "beforeafter");
        assert!(pending.is_empty());
    }

    #[test]
    fn test_filter_strips_dcs_sequence() {
        let mut pending = Vec::new();
        let text = "before\x1bP>|xterm 388\x1b\\after";
        assert_eq!(filter_text_chunk(&mut pending, text), "beforeafter");
        assert!(pending.is_empty());
    }

    #[test]
    fn test_filter_strips_split_apc_sequence() {
        let mut pending = Vec::new();
        // First chunk ends with a partial APC sequence.
        let text1 = "hello\x1b_Gi=31337";
        assert_eq!(filter_text_chunk(&mut pending, text1), "hello");
        assert_eq!(pending_text(&pending), "\x1b_Gi=31337");
        // Second chunk completes the APC with ST.
        let text2 = ";OK\x1b\\world";
        assert_eq!(filter_text_chunk(&mut pending, text2), "world");
        assert!(pending.is_empty());
    }

    #[test]
    fn test_filter_strips_split_dcs_sequence() {
        let mut pending = Vec::new();
        let text1 = "hello\x1bP>|xterm";
        assert_eq!(filter_text_chunk(&mut pending, text1), "hello");
        assert_eq!(pending_text(&pending), "\x1bP>|xterm");
        let text2 = " 388\x1b\\world";
        assert_eq!(filter_text_chunk(&mut pending, text2), "world");
        assert!(pending.is_empty());
    }

    #[test]
    fn test_has_visible_content_apc_sequence() {
        // APC sequence should not count as visible content.
        assert!(!has_visible_content("\x1b_Gi=31337;OK\x1b\\"));
        assert!(!has_visible_content(
            "\x1b_Gi=31337,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"
        ));
    }

    #[test]
    fn test_has_visible_content_dcs_sequence() {
        assert!(!has_visible_content("\x1bP>|xterm 388\x1b\\"));
    }

    #[test]
    fn test_has_visible_content_apc_with_surrounding_text() {
        assert!(has_visible_content("hello\x1b_Gi=31337;OK\x1b\\"));
        assert!(has_visible_content("\x1b_Gi=31337;OK\x1b\\world"));
    }
}
