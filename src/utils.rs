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
    format!(
        "rgb:{red:02x}{red:02x}/{green:02x}{green:02x}/{blue:02x}{blue:02x}"
    )
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
        assert_eq!(terminal_query_tail_len("text\x1b]10;?\x1b"), "\x1b]10;?\x1b".len());
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
