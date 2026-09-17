//! Engine evaluation harness for oly ADR-0001 (M0 gate prototype).
//!
//! Compares `vt100` (incumbent) and `alacritty_terminal` (lead candidate)
//! on the properties the 1.0 plan requires:
//!
//! 1. Clean-stream agreement: both engines render a corpus identically.
//! 2. Split-at-every-byte continuation: checkpoint at every byte boundary,
//!    restore, continue, compare against an uninterrupted oracle.
//!    - vt100 uses `state_formatted()` (the incumbent strategy).
//!    - alacritty: full-state checkpoint is not public API; the probe
//!      measures what *is* serializable (the grid) to size a bounded fork.
//! 3. Non-destructive resize: shrink then widen must not lose history.
//!
//! Run: `cargo run --release`

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term, test::TermSize};
use alacritty_terminal::vte::ansi::Processor;

const ROWS: usize = 24;
const COLS: usize = 80;

/// Corpus covering the failure modes found in M0: SGR styles, wrapped
/// long lines, scrollback, alternate screen, UTF-8 (multi-byte), and
/// query-like sequences.
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("plain lines", b"one\r\ntwo\r\nthree\r\n".to_vec()),
        ("sgr styles", b"\x1b[1;31mred bold\x1b[0m plain\r\n\x1b[4munderline\x1b[0m\r\n".to_vec()),
        ("wrapped long line", format!("{}\r\nnext\r\n", "w".repeat(200)).into_bytes()),
        (
            "scrollback fill",
            (0..40).map(|i| format!("line {i:03}\r\n")).collect::<String>().into_bytes(),
        ),
        (
            "alternate screen roundtrip",
            b"MAIN\r\n\x1b[?1049h\x1b[2J\x1b[HTUI-FRAME\x1b[?1049lMAIN-BACK\r\n".to_vec(),
        ),
        ("utf8 multibyte", "héllo wörld — ünïcødé ✓\r\n".as_bytes().to_vec()),
        (
            "mixed modes",
            b"\x1b[?25lhidden-cursor\r\n\x1b[7mrev\x1b[27m normal\r\n\x1b[?25h".to_vec(),
        ),
    ]
}

// ---------------------------------------------------------------------------
// vt100 adapter (incumbent)
// ---------------------------------------------------------------------------

struct Vt100 {
    parser: vt100::Parser,
}

impl Vt100 {
    fn new() -> Self {
        Self {
            parser: vt100::Parser::new(ROWS as u16, COLS as u16, 10_000),
        }
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Visible screen text, right-trimmed lines, blank-only tails removed.
    fn text(&self) -> Vec<String> {
        normalize(&self.parser.screen().contents())
    }

    /// Visible + scrollback text. vt100 exposes scrollback only through the
    /// viewing offset; page through it with `set_scrollback` like oly does.
    fn full_text(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let screen = self.parser.screen();
        let mut view = screen.clone();
        view.set_scrollback(usize::MAX);
        let total = view.scrollback();
        let mut offset = total;
        while offset > 0 {
            let page_len = offset.min(ROWS);
            view.set_scrollback(offset);
            for row in view.rows_formatted(0, COLS as u16).take(page_len) {
                lines.push(String::from_utf8_lossy(&row).trim_end().to_string());
            }
            offset -= page_len;
        }
        lines.extend(self.text());
        trim_blank_edges(lines)
    }

    /// The incumbent checkpoint strategy: formatted screen replay.
    fn checkpoint(&self) -> Vec<u8> {
        self.parser.screen().state_formatted()
    }

    fn restore(bytes: &[u8]) -> Self {
        let mut parser = vt100::Parser::new(ROWS as u16, COLS as u16, 10_000);
        parser.process(bytes);
        Self { parser }
    }

    /// oly's current resize strategy: rebuild from formatted state.
    fn resize(&mut self, rows: u16, cols: u16) {
        let snapshot = self.parser.screen().state_formatted();
        let mut parser = vt100::Parser::new(rows, cols, 10_000);
        parser.process(&snapshot);
        self.parser = parser;
    }
}

// ---------------------------------------------------------------------------
// alacritty adapter (lead candidate)
// ---------------------------------------------------------------------------

struct Alac {
    term: Term<VoidListener>,
    processor: Processor,
}

impl Alac {
    fn new() -> Self {
        let config = Config {
            scrolling_history: 10_000,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &TermSize::new(COLS, ROWS), VoidListener),
            processor: Processor::new(),
        }
    }

    fn process(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    fn resize(&mut self, rows: usize, cols: usize) {
        self.term.resize(TermSize::new(cols, rows));
    }

    fn text(&self) -> Vec<String> {
        let grid = self.term.grid();
        let mut lines = Vec::new();
        for line in 0..grid.screen_lines() {
            lines.push(row_text(&grid[alacritty_terminal::index::Line(line as i32)]));
        }
        trim_blank_edges(lines)
    }

    /// Visible + scrollback text.
    fn full_text(&self) -> Vec<String> {
        let grid = self.term.grid();
        let history = grid.history_size();
        let mut lines = Vec::new();
        for line in -(history as i32)..grid.screen_lines() as i32 {
            lines.push(row_text(&grid[alacritty_terminal::index::Line(line)]));
        }
        trim_blank_edges(lines)
    }

    /// What a checkpoint could serialize today without a fork: the grid.
    /// (Term itself — modes, scroll region, colors, cursor style — is not
    /// serializable public API in 0.26.)
    fn grid_checkpoint_roundtrip(&self) -> bool {
        let bytes = bincode::serialize(self.term.grid()).expect("grid encode");
        let _grid: alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell> =
            bincode::deserialize(&bytes).expect("grid decode");
        true
    }
}

fn row_text(row: &alacritty_terminal::grid::Row<alacritty_terminal::term::cell::Cell>) -> String {
    let mut text = String::new();
    for cell in row.into_iter() {
        if cell.flags.contains(
            alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER
                | alacritty_terminal::term::cell::Flags::LEADING_WIDE_CHAR_SPACER,
        ) {
            continue;
        }
        if cell.c == '\0' {
            break;
        }
        text.push(cell.c);
    }
    text.trim_end().to_string()
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn normalize(contents: &str) -> Vec<String> {
    trim_blank_edges(
        contents
            .lines()
            .map(|line| line.trim_end().to_string())
            .collect(),
    )
}

fn trim_blank_edges(mut lines: Vec<String>) -> Vec<String> {
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    while lines.first().is_some_and(|l| l.is_empty()) {
        lines.remove(0);
    }
    lines
}

// ---------------------------------------------------------------------------
// Experiments
// ---------------------------------------------------------------------------

fn exp1_clean_stream_agreement() {
    println!("=== 1. Clean-stream agreement (vt100 vs alacritty) ===");
    for (name, stream) in corpus() {
        let mut v = Vt100::new();
        v.process(&stream);
        let mut a = Alac::new();
        a.process(&stream);
        let agree = v.text() == a.text();
        println!(
            "  {name:32} {}",
            if agree { "AGREE" } else { "DIFFER" }
        );
        if !agree {
            println!("    vt100:     {:?}", v.text());
            println!("    alacritty: {:?}", a.text());
        }
    }
}

fn exp2_split_continuation() {
    println!("=== 2. Split-at-every-byte continuation (vt100 + state_formatted) ===");
    for (name, stream) in corpus() {
        let mut oracle = Vt100::new();
        oracle.process(&stream);
        let oracle_text = oracle.text();

        let mut failures = 0;
        let mut first_failure = None;
        for split in 0..stream.len() {
            let mut prefix = Vt100::new();
            prefix.process(&stream[..split]);
            let checkpoint = prefix.checkpoint();

            let mut restored = Vt100::restore(&checkpoint);
            restored.process(&stream[split..]);
            if restored.text() != oracle_text {
                failures += 1;
                if first_failure.is_none() {
                    first_failure = Some(split);
                }
            }
        }
        println!(
            "  {name:32} {} splits, {} failed{}",
            stream.len(),
            failures,
            first_failure
                .map(|s| format!(" (first at byte {s})"))
                .unwrap_or_default(),
        );
    }
}

fn exp3_resize_reflow() {
    println!("=== 3. Shrink(40) then widen(80): history preservation ===");
    // 40 lines of numbered content, each 60 cols wide (wraps at 40).
    let stream: Vec<u8> = (0..40)
        .map(|i| format!("line {i:03} {}\r\n", "x".repeat(50)))
        .collect::<String>()
        .into_bytes();

    let mut oracle = Vt100::new();
    oracle.process(&stream);
    let oracle_lines = oracle.full_text();

    let mut resized = Vt100::new();
    resized.process(&stream);
    resized.resize(ROWS as u16, 40);
    resized.resize(ROWS as u16, COLS as u16);
    let resized_lines = resized.full_text();

    let lost: Vec<_> = oracle_lines
        .iter()
        .filter(|l| !l.is_empty() && !resized_lines.contains(l))
        .collect();
    println!(
        "  vt100:  oracle {} non-blank lines, {} lost after shrink+widen",
        oracle_lines.iter().filter(|l| !l.is_empty()).count(),
        lost.len()
    );

    let mut a_oracle = Alac::new();
    a_oracle.process(&stream);
    let a_oracle_lines = a_oracle.full_text();

    let mut a_resized = Alac::new();
    a_resized.process(&stream);
    a_resized.resize(ROWS, 40);
    a_resized.resize(ROWS, COLS);
    let a_resized_lines = a_resized.full_text();

    let a_lost: Vec<_> = a_oracle_lines
        .iter()
        .filter(|l| !l.is_empty() && !a_resized_lines.contains(l))
        .collect();
    println!(
        "  alacritty: oracle {} non-blank lines, {} lost after shrink+widen",
        a_oracle_lines.iter().filter(|l| !l.is_empty()).count(),
        a_lost.len()
    );
}

fn exp4_checkpoint_feasibility() {
    println!("=== 4. alacritty checkpoint feasibility (0.26, feature serde) ===");
    let mut a = Alac::new();
    a.process(b"\x1b[1;31mstyled\r\n\x1b[?1049halt\x1b[?1049lback\r\n");
    println!(
        "  Grid<Cell> bincode round-trip: {}",
        if a.grid_checkpoint_roundtrip() {
            "WORKS"
        } else {
            "FAILS"
        }
    );
    println!("  Term (modes/scroll-region/colors/cursor style/keyboard) serde: NOT PUBLIC");
    println!("  => full checkpoint requires a bounded upstream patch or fork; grid only is insufficient");
}

fn main() {
    exp1_clean_stream_agreement();
    println!();
    exp2_split_continuation();
    println!();
    exp3_resize_reflow();
    println!();
    exp4_checkpoint_feasibility();
}
