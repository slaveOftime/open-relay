// ---------------------------------------------------------------------------
// PTY-related types and terminal query/escape handling
// ---------------------------------------------------------------------------

use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// PtyHandle — owns the PTY file descriptors, reader/writer threads, and child
// ---------------------------------------------------------------------------

/// Pure PTY ownership struct. Manages the master fd, the child process, and
/// the dedicated reader/writer threads.  No business logic (notifications,
/// session metadata, etc.) lives here.
pub struct PtyHandle {
    pub(crate) child: RuntimeChild,
    /// Channel to the dedicated PTY writer thread.
    pub(crate) writer_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Kept alive so the master fd stays open; resize goes through this.
    pub(crate) pty_master: Option<Box<dyn portable_pty::MasterPty + Send>>,
}

impl PtyHandle {
    /// Send raw bytes to the child's stdin via the writer thread.
    pub fn write_input(&self, data: Vec<u8>) -> bool {
        self.writer_tx.send(data).is_ok()
    }

    /// Resize the PTY. Returns `true` on success.
    pub fn resize(&mut self, rows: u16, cols: u16) -> bool {
        if rows == 0 || cols == 0 {
            return false;
        }
        let Some(master) = self.pty_master.as_mut() else {
            return false;
        };
        master
            .resize(portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .is_ok()
    }

    /// Send SIGKILL / close ConPTY.
    pub fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill()
    }

    /// Non-blocking check for child exit. Returns `Some(exit_code)` if exited.
    pub fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        self.child.try_wait_code()
    }

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

// ---------------------------------------------------------------------------
// Terminal query types and helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalQuery {
    CursorPositionReport,
    DeviceStatusReport,
    ForegroundColor,
    BackgroundColor,
}

const TERMINAL_QUERY_PATTERNS: [(&str, TerminalQuery); 6] = [
    ("\x1b[6n", TerminalQuery::CursorPositionReport),
    ("\x1b[5n", TerminalQuery::DeviceStatusReport),
    ("\x1b]10;?\x07", TerminalQuery::ForegroundColor),
    ("\x1b]10;?\x1b\\", TerminalQuery::ForegroundColor),
    ("\x1b]11;?\x07", TerminalQuery::BackgroundColor),
    ("\x1b]11;?\x1b\\", TerminalQuery::BackgroundColor),
];

pub fn find_next_terminal_query(
    text: &str,
    search_from: usize,
) -> Option<(usize, usize, TerminalQuery)> {
    TERMINAL_QUERY_PATTERNS
        .iter()
        .filter_map(|(pattern, query)| {
            text[search_from..]
                .find(pattern)
                .map(|offset| (search_from + offset, pattern.len(), *query))
        })
        .min_by_key(|(start, _, _)| *start)
}

pub fn terminal_query_tail_len(remainder: &str) -> usize {
    let mut keep = 0usize;
    for (pattern, _) in TERMINAL_QUERY_PATTERNS {
        let max_prefix = pattern.len().saturating_sub(1).min(remainder.len());
        for prefix_len in (1..=max_prefix).rev() {
            if remainder.ends_with(&pattern[..prefix_len]) {
                keep = keep.max(prefix_len);
                break;
            }
        }
    }
    keep
}

pub fn terminal_query_response(query: TerminalQuery, cursor: Option<(u16, u16)>) -> String {
    match query {
        TerminalQuery::CursorPositionReport => {
            let (row, col) = cursor.unwrap_or((1, 1));
            format!("\x1b[{row};{col}R")
        }
        TerminalQuery::DeviceStatusReport => "\x1b[0n".to_string(),
        TerminalQuery::ForegroundColor => {
            let (foreground, _) = terminal_report_colors();
            format_osc_color_response(10, &foreground)
        }
        TerminalQuery::BackgroundColor => {
            let (_, background) = terminal_report_colors();
            format_osc_color_response(11, &background)
        }
    }
}

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

fn format_osc_color_response(ps: u8, color: &str) -> String {
    format!("\x1b]{ps};{color}\x1b\\")
}

fn format_osc_rgb((red, green, blue): (u8, u8, u8)) -> String {
    format!("rgb:{red:02x}{red:02x}/{green:02x}{green:02x}/{blue:02x}{blue:02x}")
}

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

/// Strips CPR/DSR terminal device responses and OSC 10/11 color responses
/// from PTY output before display.  Carries incomplete sequences across chunk
/// boundaries so ConPTY split-ESC cases are handled correctly.
///
/// All regex state is static (shared); only the cross-chunk `pending` prefix
/// is per-instance.
pub struct EscapeFilter {
    pending: String,
}

impl EscapeFilter {
    pub fn new() -> Self {
        Self {
            pending: String::new(),
        }
    }

    /// Filter a raw PTY byte slice and return the cleaned bytes.
    pub fn filter(&mut self, data: &[u8]) -> Vec<u8> {
        let chunk = String::from_utf8_lossy(data);
        filter_cpr_chunk(&mut self.pending, &chunk).into_bytes()
    }
}

impl Default for EscapeFilter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Daemon-side fallback query responder (no attach client)
// ---------------------------------------------------------------------------

/// Scan `data` for DSR/CPR/OSC colour queries and generate response bytes.
///
/// Responses are for the daemon to write back to the PTY stdin when no attach
/// client is connected.  CPR always uses row 1 col 1 (the best the daemon can
/// do without a real terminal).  `tail` carries partial query sequences across
/// chunk boundaries.
///
/// Returns a list of response byte strings to write sequentially.
///
/// OSC 10/11 colour probes are answered with a static best-guess response
/// (white foreground, black background, or the `COLORFGBG` env var if set).
/// Any echoed colour response that the child app writes back to its own stdout
/// is stripped by the `EscapeFilter` (via `GENERIC_OSC_FULL_RE`) before being
/// forwarded to attach clients, so there is no visible junk.
pub fn extract_query_responses_no_client(data: &[u8], tail: &mut String) -> Vec<Vec<u8>> {
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
        match query {
            TerminalQuery::CursorPositionReport | TerminalQuery::DeviceStatusReport => {
                let response = terminal_query_response(query, Some((1, 1)));
                responses.push(response.into_bytes());
            }
            TerminalQuery::ForegroundColor | TerminalQuery::BackgroundColor => {
                let response = terminal_query_response(query, None);
                responses.push(response.into_bytes());
            }
        }
        search_from = match_start + query_len;
    }

    let remainder = &combined[search_from..];
    let keep = terminal_query_tail_len(remainder);
    *tail = remainder[remainder.len().saturating_sub(keep)..].to_string();

    responses
}

// ---------------------------------------------------------------------------
// filter_cpr_chunk — shared ESC-response filter (used by EscapeFilter)
// ---------------------------------------------------------------------------

/// Filters CPR (Cursor Position Report) and OSC 10/11 color responses out of
/// a single PTY chunk.
///
/// ConPTY on Windows echoes device/color responses into the master output
/// stream.  The sequence is frequently split across PTY read boundaries at
/// *any* byte, so `pending` carries a trailing prefix from one call to the
/// next to ensure every fragment is reassembled before being examined.
///
/// Splitting points handled (all stripped):
/// * Full CPR in one chunk:   `\x1b[35;1R`
/// * ESC alone:               `…\x1b`  |  `[35;1R…`
/// * Bare (no ESC from ConPTY): `…[35;1R…`
/// * OSC 10/11 full or bare variants
pub fn filter_cpr_chunk(pending: &mut String, chunk: &str) -> String {
    use std::sync::OnceLock;

    static PARTIAL_RE: OnceLock<regex::Regex> = OnceLock::new();
    let partial_re =
        PARTIAL_RE.get_or_init(|| regex::Regex::new(r"\x1b(?:\[(?:\??\d*(?:;\d*)?)?)?$").unwrap());

    static FULL_RE: OnceLock<regex::Regex> = OnceLock::new();
    let full_re = FULL_RE.get_or_init(|| regex::Regex::new(r"\x1b\[\??\d+;\d+R").unwrap());

    static BARE_RE: OnceLock<regex::Regex> = OnceLock::new();
    let bare_re = BARE_RE.get_or_init(|| regex::Regex::new(r"\[\??\d+;\d+R").unwrap());

    static OSC_FULL_RE: OnceLock<regex::Regex> = OnceLock::new();
    let osc_full_re = OSC_FULL_RE.get_or_init(|| {
        regex::Regex::new(
            r"\x1b]1(?:0|1);rgb:[0-9A-Fa-f]{4}/[0-9A-Fa-f]{4}/[0-9A-Fa-f]{4}(?:\x07|\x1b\\)",
        )
        .unwrap()
    });

    static OSC_BARE_RE: OnceLock<regex::Regex> = OnceLock::new();
    let osc_bare_re = OSC_BARE_RE.get_or_init(|| {
        regex::Regex::new(r"]1(?:0|1);rgb:[0-9A-Fa-f]{4}/[0-9A-Fa-f]{4}/[0-9A-Fa-f]{4}(?:\x07|\\)")
            .unwrap()
    });

    static GENERIC_OSC_FULL_RE: OnceLock<regex::Regex> = OnceLock::new();
    let generic_osc_full_re = GENERIC_OSC_FULL_RE.get_or_init(|| {
        regex::Regex::new(r"\x1b]\d{1,3}(?:;[^\x07\x1b]*)*(?:\x07|\x1b\\)").unwrap()
    });

    static GENERIC_OSC_BARE_RE: OnceLock<regex::Regex> = OnceLock::new();
    let generic_osc_bare_re = GENERIC_OSC_BARE_RE
        .get_or_init(|| regex::Regex::new(r"]\d{1,3}(?:;[^\x07\\]*)*(?:\x07|\\)").unwrap());

    // 1. Prepend any fragment saved from the previous chunk.
    let mut combined = std::mem::take(pending);
    combined.push_str(chunk);

    // 2. Save any trailing incomplete CPR/OSC prefix for the next call.
    let cpr_pending_start = partial_re.find(&combined).map(|m| m.start());
    let osc_pending_start = trailing_partial_osc_sequence_start(&combined);
    if let Some(start) = [cpr_pending_start, osc_pending_start]
        .into_iter()
        .flatten()
        .min()
    {
        *pending = combined[start..].to_string();
        combined.truncate(start);
    }

    // 3. Strip fully-formed ESC-prefixed CPRs.
    let combined = full_re.replace_all(&combined, "");

    // 4. Strip bare CPRs (ESC missing / dropped by ConPTY).
    let combined = bare_re.replace_all(&combined, "");

    // 5. Strip OSC 10/11 color responses, ESC-prefixed or bare.
    let combined = osc_full_re.replace_all(&combined, "");
    let combined = osc_bare_re.replace_all(&combined, "");

    // 6. Strip generic OSC control sequences such as OSC 7 shell cwd updates.
    let combined = generic_osc_full_re.replace_all(&combined, "");
    generic_osc_bare_re.replace_all(&combined, "").into_owned()
}

pub(crate) fn trailing_partial_osc_sequence_start(text: &str) -> Option<usize> {
    ["\x1b]10;rgb:", "\x1b]11;rgb:", "]10;rgb:", "]11;rgb:"]
        .into_iter()
        .filter_map(|prefix| text.rfind(prefix))
        .filter(|start| is_partial_osc_color_response(&text[*start..]))
        .chain(
            ["\x1b]", "]"]
                .into_iter()
                .filter_map(|prefix| text.rfind(prefix))
                .filter(|start| is_partial_generic_osc_sequence(&text[*start..])),
        )
        .min()
}

pub(crate) fn is_partial_osc_color_response(candidate: &str) -> bool {
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
            0x07 => return false,
            0x1b => {
                return if index + 1 == bytes.len() {
                    slash_count == 2 && saw_hex
                } else {
                    bytes.get(index + 1) != Some(&b'\\')
                };
            }
            b'\\' => return false,
            _ => return false,
        }
    }
    true
}

pub(crate) fn is_partial_generic_osc_sequence(candidate: &str) -> bool {
    let bytes = candidate.as_bytes();
    let mut index = 0usize;

    if bytes.first() == Some(&0x1b) {
        index += 1;
    }
    if bytes.get(index) != Some(&b']') {
        return false;
    }
    index += 1;

    let digits_start = index;
    while let Some(&byte) = bytes.get(index) {
        if byte.is_ascii_digit() {
            index += 1;
            continue;
        }
        break;
    }

    if index == digits_start || index - digits_start > 3 {
        return false;
    }
    if bytes.get(index) != Some(&b';') {
        return false;
    }
    index += 1;

    while let Some(&byte) = bytes.get(index) {
        match byte {
            0x07 => return false,
            0x1b => {
                return if index + 1 == bytes.len() {
                    true
                } else {
                    bytes.get(index + 1) != Some(&b'\\')
                };
            }
            b'\\' => return false,
            _ => index += 1,
        }
    }

    true
}

// ---------------------------------------------------------------------------
// has_visible_content
// ---------------------------------------------------------------------------

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
    fn test_xterm_color_to_rgb_cube_and_grayscale() {
        assert_eq!(xterm_color_to_rgb(16), (0x00, 0x00, 0x00));
        assert_eq!(xterm_color_to_rgb(21), (0x00, 0x00, 0xff));
        assert_eq!(xterm_color_to_rgb(232), (0x08, 0x08, 0x08));
        assert_eq!(xterm_color_to_rgb(255), (0xee, 0xee, 0xee));
    }

    #[test]
    fn test_filter_cpr_chunk_strips_generic_osc_sequences() {
        let mut pending = String::new();
        let text = "before\x1b]7;file://host/home/binwen/open-relay/target/debug\x07after";
        assert_eq!(filter_cpr_chunk(&mut pending, text), "beforeafter");
        assert!(pending.is_empty());
    }

    #[test]
    fn test_filter_cpr_chunk_keeps_partial_generic_osc_sequence_pending() {
        let mut pending = String::new();
        let text = "before\x1b]7;file://host/home/binwen/open-relay/target/debug";
        assert_eq!(filter_cpr_chunk(&mut pending, text), "before");
        assert_eq!(
            pending,
            "\x1b]7;file://host/home/binwen/open-relay/target/debug"
        );
    }
}
