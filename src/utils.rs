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

pub fn get_base_url(endpoint: &str) -> String {
    if let Ok(url) = reqwest::Url::parse(endpoint) {
        let mut origin = format!("{}://{}", url.scheme(), url.host_str().unwrap_or_default());
        if let Some(port) = url.port() {
            origin.push(':');
            origin.push_str(&port.to_string());
        }
        return origin;
    }
    endpoint.to_string()
}

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
// TerminalModeTracker — tracks bracketed paste and DECCKM from raw bytes
// ---------------------------------------------------------------------------

/// Tracks DEC private mode toggles emitted by the child PTY process.
/// Feed each raw output chunk through `process()` to keep state current.
#[allow(dead_code)]
pub struct TerminalModeTracker {
    pub bracketed_paste_mode: bool,
    pub app_cursor_keys: bool,
}

#[allow(dead_code)]
impl TerminalModeTracker {
    pub fn new() -> Self {
        Self {
            bracketed_paste_mode: false,
            app_cursor_keys: false,
        }
    }

    /// Process a raw PTY output chunk and update tracked modes.
    pub fn process(&mut self, data: &[u8]) {
        let chunk = String::from_utf8_lossy(data);
        if chunk.contains("\x1b[?2004h") {
            self.bracketed_paste_mode = true;
        } else if chunk.contains("\x1b[?2004l") {
            self.bracketed_paste_mode = false;
        }
        if chunk.contains("\x1b[?1h") {
            self.app_cursor_keys = true;
        } else if chunk.contains("\x1b[?1l") {
            self.app_cursor_keys = false;
        }
    }
}

impl Default for TerminalModeTracker {
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
        let response = terminal_query_response(query, Some((1, 1)));
        responses.push(response.into_bytes());
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

    // 1. Prepend any fragment saved from the previous chunk.
    let mut combined = std::mem::take(pending);
    combined.push_str(chunk);

    // 2. Save any trailing incomplete CPR/OSC prefix for the next call.
    let cpr_pending_start = partial_re.find(&combined).map(|m| m.start());
    let osc_pending_start = trailing_partial_osc_response_start(&combined);
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
    osc_bare_re.replace_all(&combined, "").into_owned()
}

pub(crate) fn trailing_partial_osc_response_start(text: &str) -> Option<usize> {
    ["\x1b]10;rgb:", "\x1b]11;rgb:", "]10;rgb:", "]11;rgb:"]
        .into_iter()
        .filter_map(|prefix| text.rfind(prefix))
        .filter(|start| is_partial_osc_color_response(&text[*start..]))
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
            0x07 => return slash_count == 2 && saw_hex,
            0x1b => return index + 1 == bytes.len(),
            b'\\' => return false,
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
