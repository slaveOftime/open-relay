use std::panic::{AssertUnwindSafe, catch_unwind};

use tracing::warn;

pub fn safe_resize_parser(parser: &mut vt100::Parser, rows: u16, cols: u16) {
    let resize = catch_unwind(AssertUnwindSafe(|| {
        parser.screen_mut().set_size(rows, cols);
    }));

    if resize.is_ok() {
        return;
    }

    let snapshot = parser.screen().state_formatted();
    warn!(
        rows,
        cols, "vt100 parser resize panicked; rebuilding parser from snapshot"
    );

    let mut rebuilt = vt100::Parser::new(rows, cols, 0);
    if !snapshot.is_empty() {
        rebuilt.process(&snapshot);
    }
    *parser = rebuilt;
}

#[cfg(test)]
mod tests {
    use super::safe_resize_parser;

    fn parser_contents(rows: u16, cols: u16, data: &[u8]) -> String {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(data);
        parser.screen().contents()
    }

    #[test]
    fn safe_resize_preserves_visible_content_and_modes() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[?1049h\x1b[2J\x1b[Hhello\x1b[?2004h");

        safe_resize_parser(&mut parser, 34, 44);

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

        safe_resize_parser(&mut parser, 20, 44);

        let screen = parser.screen();
        assert_eq!(screen.size(), (20, 44));
        assert!(screen.contents().contains('中'));
    }

    #[test]
    fn resized_snapshot_rehydrates_into_fresh_parser() {
        let mut parser = vt100::Parser::new(10, 60, 0);
        parser.process(b"\x1b[?1049h\x1b[2J\x1b[H12345");
        safe_resize_parser(&mut parser, 10, 5);

        let contents = parser_contents(10, 5, &parser.screen().state_formatted());
        assert!(contents.contains("12345"));
    }
}
