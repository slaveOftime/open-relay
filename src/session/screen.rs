//! Helpers for driving the `vt100` terminal screen parser.
//!
//! Deliberately *not* named `vt100`: this module and the crate it wraps would
//! otherwise share a name, and callers such as [`super::runtime`] refer to both
//! in the same scope.

use std::panic::{AssertUnwindSafe, catch_unwind};

use tracing::warn;

/// Rebuild the parser at a new size, preserving the visible state and up to
/// `scrollback_rows` of scrolled-off history (see `scrollback_dump`).
pub fn safe_resize_parser(
    parser: &mut vt100::Parser,
    rows: u16,
    cols: u16,
    scrollback_rows: usize,
) {
    if parser.screen().size() == (rows, cols) {
        return;
    }

    let snapshot = parser.screen().state_formatted();
    let mut scrollback = scrollback_dump(parser.screen());
    if !scrollback.is_empty() {
        // Push every dumped row past the visible area: the snapshot's leading
        // clear-screen would otherwise erase dump rows still sitting on the
        // grid of a parser shorter than the dump.
        scrollback.resize(scrollback.len() + usize::from(rows), b'\n');
    }
    let rebuild = catch_unwind(AssertUnwindSafe(|| {
        let mut rebuilt = vt100::Parser::new(rows, cols, scrollback_rows);
        // Re-feed scrolled-off rows as plain lines so they land in the new
        // parser's scrollback before the snapshot repaints the visible grid.
        if !scrollback.is_empty() {
            rebuilt.process(&scrollback);
        }
        if !snapshot.is_empty() {
            rebuilt.process(&snapshot);
        }
        *parser = rebuilt;
    }));

    if rebuild.is_err() {
        warn!(
            rows,
            cols, "vt100 parser resize rebuild panicked; resetting parser"
        );
        *parser = vt100::Parser::new(rows, cols, scrollback_rows);
    }
}

/// Collect the rows that scrolled off the visible screen, oldest first.
///
/// vt100 exposes scrollback only through the viewing offset, and one scrolled
/// view yields at most a screenful of scrollback rows, so this pages from the
/// oldest retained row forward.  While the alternate screen is active vt100
/// exposes only the alternate grid (which never has scrollback), so the
/// result is empty and a resize during a full-screen TUI drops the pre-TUI
/// history.
pub fn scrollback_rows(screen: &vt100::Screen) -> Vec<Vec<u8>> {
    let (rows_len, cols) = screen.size();
    let page_height = usize::from(rows_len).max(1);
    let mut view = screen.clone();
    view.set_scrollback(usize::MAX);
    let total = view.scrollback();
    let mut rows = Vec::with_capacity(total);
    let mut offset = total;
    while offset > 0 {
        let page_len = offset.min(page_height);
        view.set_scrollback(offset);
        rows.extend(view.rows_formatted(0, cols).take(page_len));
        offset -= page_len;
    }
    rows
}

/// Serialize the rows that scrolled off the visible screen as formatted
/// CRLF-terminated lines, oldest first, so they can be re-fed into a rebuilt
/// parser.
fn scrollback_dump(screen: &vt100::Screen) -> Vec<u8> {
    let mut dump = Vec::new();
    for row in scrollback_rows(screen) {
        dump.extend_from_slice(&row);
        dump.extend_from_slice(b"\r\n");
    }
    dump
}

#[cfg(test)]
mod tests {
    use super::{safe_resize_parser, scrollback_dump};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    const TEST_SCROLLBACK_ROWS: usize = 100;

    fn parser_contents(rows: u16, cols: u16, data: &[u8]) -> String {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(data);
        parser.screen().contents()
    }

    #[test]
    fn safe_resize_preserves_visible_content_and_modes() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[?1049h\x1b[2J\x1b[Hhello\x1b[?2004h");

        safe_resize_parser(&mut parser, 34, 44, TEST_SCROLLBACK_ROWS);

        let screen = parser.screen();
        assert_eq!(screen.size(), (34, 44));
        assert!(screen.contents().contains("hello"));
        assert!(
            screen
                .state_formatted()
                .windows(8)
                .any(|window| window == b"\x1b[?2004h")
        );
    }

    #[test]
    fn safe_resize_handles_wide_glyphs_near_right_edge() {
        let mut parser = vt100::Parser::new(12, 80, 0);
        let bytes = format!("\x1b[2J\x1b[H{}中", "x".repeat(43));
        parser.process(bytes.as_bytes());

        safe_resize_parser(&mut parser, 20, 44, TEST_SCROLLBACK_ROWS);

        let screen = parser.screen();
        assert_eq!(screen.size(), (20, 44));
        assert!(screen.contents().contains('中'));
    }

    #[test]
    fn resized_snapshot_rehydrates_into_fresh_parser() {
        let mut parser = vt100::Parser::new(10, 60, 0);
        parser.process(b"\x1b[?1049h\x1b[2J\x1b[H12345");
        safe_resize_parser(&mut parser, 10, 5, TEST_SCROLLBACK_ROWS);

        let contents = parser_contents(10, 5, &parser.screen().state_formatted());
        assert!(contents.contains("12345"));
    }

    #[test]
    fn safe_resize_preserves_scrolled_off_rows() {
        let mut parser = vt100::Parser::new(5, 80, TEST_SCROLLBACK_ROWS);
        let mut lines = String::new();
        for i in 1..=20 {
            lines.push_str(&format!("line {i}\r\n"));
        }
        parser.process(lines.as_bytes());
        // The first rows have scrolled off the 5-row screen into scrollback.
        assert!(!parser.screen().contents().contains("line 1\n"));

        safe_resize_parser(&mut parser, 8, 40, TEST_SCROLLBACK_ROWS);

        let dump = scrollback_dump(parser.screen());
        let history = String::from_utf8_lossy(&dump);
        assert!(history.contains("line 1"));
        assert!(history.contains("line 15"));
        assert!(parser.screen().contents().contains("line 20"));
    }

    #[test]
    fn safe_resize_handles_wide_glyph_at_new_right_edge() {
        let mut parser = vt100::Parser::new(42, 120, 0);
        let bytes = format!("{}中", "x".repeat(99));
        parser.process(bytes.as_bytes());

        safe_resize_parser(&mut parser, 42, 100, TEST_SCROLLBACK_ROWS);

        let result = catch_unwind(AssertUnwindSafe(|| {
            parser.process(b"\x1b[K\x1b[1K\x1b[P\x1b[@after resize");
        }));

        assert!(result.is_ok());
        assert_eq!(parser.screen().size(), (42, 100));
    }
}
