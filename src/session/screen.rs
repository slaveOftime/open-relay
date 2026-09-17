//! Helpers for driving the `vt100` terminal screen parser.
//!
//! Deliberately *not* named `vt100`: this module and the crate it wraps would
//! otherwise share a name, and callers such as [`super::runtime`] refer to both
//! in the same scope.

use std::panic::{AssertUnwindSafe, catch_unwind};

use tracing::warn;

/// Rebuild the parser at a new size, preserving the visible state and up to
/// `scrollback_rows` of scrolled-off history (see `scrollback_dump`).
///
/// The dumped rows are trimmed to the *new* width: re-feeding a row wider
/// than the target grid would wrap it into two rows, doubling the scrollback
/// and scrambling the retained history on every column-changing resize.
pub fn safe_resize_parser(
    parser: &mut vt100::Parser,
    rows: u16,
    cols: u16,
    scrollback_rows: usize,
) {
    if parser.screen().size() == (rows, cols) {
        return;
    }

    let old_cols = parser.screen().size().1;
    let snapshot = parser.screen().state_formatted();
    let mut scrollback = scrollback_dump(parser.screen(), old_cols.min(cols));
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

/// Collect at most `tail` of the rows that scrolled off the visible screen,
/// newest last, formatted at `min(screen width, cols_limit)` columns.
///
/// vt100 exposes scrollback only through the viewing offset, and one scrolled
/// view yields at most a screenful of scrollback rows, so this pages through
/// the viewing offsets.  Only the pages covering the requested tail are
/// formatted: an attach seed wants the newest `tail` rows, so paging from the
/// newest end avoids formatting all 5000 retained rows just to keep the last
/// few.  While the alternate screen is active vt100 exposes only the alternate
/// grid (which never has scrollback), so the result is empty.
pub fn scrollback_rows_tail(screen: &vt100::Screen, tail: usize, cols_limit: u16) -> Vec<Vec<u8>> {
    let (rows_len, cols) = screen.size();
    let cols = cols.min(cols_limit);
    let page_height = usize::from(rows_len).max(1);
    let mut view = screen.clone();
    view.set_scrollback(usize::MAX);
    let total = view.scrollback();
    let take = tail.min(total);
    let mut rows = Vec::with_capacity(take);
    let mut offset = take;
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
/// parser.  `cols_limit` trims each dumped row so re-feeding it into a
/// narrower grid cannot wrap it into a doubled, scrambled scrollback.
fn scrollback_dump(screen: &vt100::Screen, cols_limit: u16) -> Vec<u8> {
    let mut dump = Vec::new();
    for row in scrollback_rows_tail(screen, usize::MAX, cols_limit) {
        dump.extend_from_slice(&row);
        dump.extend_from_slice(b"\r\n");
    }
    dump
}

#[cfg(test)]
mod tests {
    use super::{safe_resize_parser, scrollback_dump, scrollback_rows_tail};
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

        let dump = scrollback_dump(parser.screen(), 40);
        let history = String::from_utf8_lossy(&dump);
        assert!(history.contains("line 1"));
        assert!(history.contains("line 15"));
        assert!(parser.screen().contents().contains("line 20"));
    }

    #[test]
    fn scrollback_rows_tail_collects_only_the_newest_rows() {
        // 10 lines on a 3-row screen: the trailing line feeds scroll lines
        // 1..=8 off the visible screen.
        let mut parser = vt100::Parser::new(3, 80, 100);
        let mut lines = String::new();
        for i in 1..=10 {
            lines.push_str(&format!("row {i:02}\r\n"));
        }
        parser.process(lines.as_bytes());

        let rows = scrollback_rows_tail(parser.screen(), 4, 80);
        let text: Vec<String> = rows
            .iter()
            .map(|row| String::from_utf8_lossy(row).trim_end().to_string())
            .collect();
        assert_eq!(text, vec!["row 05", "row 06", "row 07", "row 08"]);
    }

    #[test]
    fn scrollback_rows_tail_limits_formatted_columns() {
        // 5 lines on a 3-row screen: scrollback holds 3 rows of 10 columns.
        let mut parser = vt100::Parser::new(3, 80, 100);
        parser.process(b"0123456789\r\n0123456789\r\n0123456789\r\n0123456789\r\n0123456789\r\n");

        let rows = scrollback_rows_tail(parser.screen(), 10, 4);
        let text: Vec<String> = rows
            .iter()
            .map(|row| String::from_utf8_lossy(row).trim_end().to_string())
            .collect();
        assert_eq!(text, vec!["0123", "0123", "0123"]);
    }

    #[test]
    fn safe_resize_shrink_does_not_wrap_scrollback_rows() {
        // 20 lines of exactly 80 columns on a 6-row screen: the trailing
        // line feeds scroll 15 full-width rows off. Shrinking to 40 columns
        // must trim each row, not wrap it: wrapping would double the
        // scrollback and scramble which line is which.
        let mut parser = vt100::Parser::new(6, 80, TEST_SCROLLBACK_ROWS);
        let mut lines = String::new();
        for i in 1..=20 {
            // "line NN" (7) + 73 filler characters = exactly 80 columns.
            lines.push_str(&format!("line {i:02}{}\r\n", "y".repeat(73)));
        }
        parser.process(lines.as_bytes());

        safe_resize_parser(&mut parser, 6, 40, TEST_SCROLLBACK_ROWS);

        let mut view = parser.screen().clone();
        view.set_scrollback(usize::MAX);
        // 15 trimmed rows plus the 6 scroll-off line feeds appended by the
        // resize rebuild. A wrapping rebuild would roughly double this.
        assert_eq!(view.scrollback(), 21, "rows must not wrap into doubles");

        // The oldest scrollback rows are still one line each, in order.
        view.set_scrollback(21);
        let contents = view.contents();
        let rows: Vec<&str> = contents.lines().collect();
        assert!(
            rows.len() >= 2,
            "expected several history rows: {contents:?}"
        );
        assert!(rows[0].starts_with("line 01"), "first row: {:?}", rows[0]);
        assert!(
            rows[1].starts_with("line 02"),
            "second row must not be a wrap continuation: {:?}",
            rows[1]
        );
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
