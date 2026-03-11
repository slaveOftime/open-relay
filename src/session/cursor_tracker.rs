/// Tracks the approximate cursor position by scanning PTY output bytes
/// through a simple state machine. Used to provide realistic CPR responses
/// when the daemon answers `\x1b[6n` on behalf of the child process.
///
/// The tracker handles:
/// * `\n` — move down one row (clamped to terminal height, simulating scroll)
/// * `\r` — move to column 1
/// * Printable characters — advance column (wrapping at terminal width)
/// * CSI `H`/`f` — absolute cursor positioning
/// * CSI `A`/`B`/`C`/`D` — relative cursor movement
/// * CSI `E`/`F` — cursor next/previous line
/// * CSI `G` — horizontal absolute
/// * CSI `d` — vertical absolute
/// * CSI `J` — erase display (2J resets position to 1,1)
/// * All other escape sequences are skipped without affecting position.
pub struct CursorTracker {
    row: u16,
    col: u16,
    rows: u16,
    cols: u16,
    state: CtState,
    params: [u16; 4],
    param_idx: usize,
    has_digit: bool,
    private_marker: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CtState {
    Normal,
    Esc,
    Csi,
    Osc,
}

impl CursorTracker {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            row: 1,
            col: 1,
            rows: rows.max(1),
            cols: cols.max(1),
            state: CtState::Normal,
            params: [0; 4],
            param_idx: 0,
            has_digit: false,
            private_marker: false,
        }
    }

    /// Current estimated cursor position (1-based row, 1-based col).
    pub fn position(&self) -> (u16, u16) {
        (self.row, self.col)
    }

    /// Update terminal dimensions (e.g. after a resize).
    #[allow(dead_code)]
    pub fn set_size(&mut self, rows: u16, cols: u16) {
        self.rows = rows.max(1);
        self.cols = cols.max(1);
        self.row = self.row.min(self.rows);
        self.col = self.col.min(self.cols);
    }

    /// Process a chunk of PTY output bytes and update cursor position.
    pub fn process(&mut self, data: &[u8]) {
        for &b in data {
            match self.state {
                CtState::Normal => self.normal(b),
                CtState::Esc => self.esc(b),
                CtState::Csi => self.csi(b),
                CtState::Osc => self.osc(b),
            }
        }
    }

    fn normal(&mut self, b: u8) {
        match b {
            0x1b => self.state = CtState::Esc,
            b'\n' => {
                if self.row < self.rows {
                    self.row += 1;
                }
            }
            b'\r' => self.col = 1,
            b'\t' => {
                let next_tab = ((self.col - 1) / 8 + 1) * 8 + 1;
                self.col = next_tab.min(self.cols);
            }
            0x20..=0x7e => {
                if self.col < self.cols {
                    self.col += 1;
                } else {
                    self.col = 1;
                    if self.row < self.rows {
                        self.row += 1;
                    }
                }
            }
            0xc0..=0xfd => {
                if self.col < self.cols {
                    self.col += 1;
                } else {
                    self.col = 1;
                    if self.row < self.rows {
                        self.row += 1;
                    }
                }
            }
            _ => {}
        }
    }

    fn esc(&mut self, b: u8) {
        match b {
            b'[' => {
                self.state = CtState::Csi;
                self.params = [0; 4];
                self.param_idx = 0;
                self.has_digit = false;
                self.private_marker = false;
            }
            b']' => self.state = CtState::Osc,
            0x1b => {}
            _ => self.state = CtState::Normal,
        }
    }

    fn csi(&mut self, b: u8) {
        match b {
            b'?' | b'>' | b'!' => {
                self.private_marker = true;
            }
            b'0'..=b'9' => {
                self.has_digit = true;
                if self.param_idx < self.params.len() {
                    self.params[self.param_idx] = self.params[self.param_idx]
                        .saturating_mul(10)
                        .saturating_add((b - b'0') as u16);
                }
            }
            b';' => {
                if self.param_idx < self.params.len() - 1 {
                    self.param_idx += 1;
                }
            }
            0x20..=0x2f => {}
            0x40..=0x7e => {
                if !self.private_marker {
                    self.execute_csi(b);
                }
                self.state = CtState::Normal;
            }
            0x1b => self.state = CtState::Esc,
            _ => self.state = CtState::Normal,
        }
    }

    fn osc(&mut self, b: u8) {
        match b {
            0x07 => self.state = CtState::Normal,
            0x1b => self.state = CtState::Esc,
            _ => {}
        }
    }

    fn execute_csi(&mut self, final_byte: u8) {
        let p0 = if self.has_digit { self.params[0] } else { 0 };
        let p1 = self.params[1.min(self.param_idx + 1)];

        match final_byte {
            b'H' | b'f' => {
                let r = if p0 == 0 { 1 } else { p0 };
                let c = if p1 == 0 { 1 } else { p1 };
                self.row = r.min(self.rows);
                self.col = c.min(self.cols);
            }
            b'A' => {
                let n = p0.max(1);
                self.row = self.row.saturating_sub(n).max(1);
            }
            b'B' => {
                let n = p0.max(1);
                self.row = (self.row + n).min(self.rows);
            }
            b'C' => {
                let n = p0.max(1);
                self.col = (self.col + n).min(self.cols);
            }
            b'D' => {
                let n = p0.max(1);
                self.col = self.col.saturating_sub(n).max(1);
            }
            b'E' => {
                let n = p0.max(1);
                self.row = (self.row + n).min(self.rows);
                self.col = 1;
            }
            b'F' => {
                let n = p0.max(1);
                self.row = self.row.saturating_sub(n).max(1);
                self.col = 1;
            }
            b'G' => {
                let c = if p0 == 0 { 1 } else { p0 };
                self.col = c.min(self.cols);
            }
            b'd' => {
                let r = if p0 == 0 { 1 } else { p0 };
                self.row = r.min(self.rows);
            }
            b'J' => {
                if p0 == 2 || p0 == 3 {
                    self.row = 1;
                    self.col = 1;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cursor_tracker_initial_position() {
        let ct = CursorTracker::new(24, 80);
        assert_eq!(ct.position(), (1, 1));
    }

    #[test]
    fn test_cursor_tracker_printable_text() {
        let mut ct = CursorTracker::new(24, 80);
        ct.process(b"hello");
        assert_eq!(ct.position(), (1, 6));
    }

    #[test]
    fn test_cursor_tracker_newline_cr() {
        let mut ct = CursorTracker::new(24, 80);
        ct.process(b"line1\r\nline2\r\n");
        assert_eq!(ct.position(), (3, 1));
    }

    #[test]
    fn test_cursor_tracker_skips_escape_sequences() {
        let mut ct = CursorTracker::new(24, 80);
        ct.process(b"\x1b[?1h\x1b=\x1b[?1h\x1b=\r\n> ");
        assert_eq!(ct.position(), (2, 3));
    }

    #[test]
    fn test_cursor_tracker_dotnet_fsi_startup() {
        let mut ct = CursorTracker::new(24, 80);
        ct.process(b"\x1b[?1h\x1b=\x1b[?1h\x1b=\r\n");
        ct.process(b"Microsoft (R) F# Interactive version 14.0.100.0 for F# 10.0\r\n");
        ct.process(b"Copyright (c) Microsoft Corporation. All Rights Reserved.\r\n");
        ct.process(b"\r\n");
        ct.process(b"For help type #help;;\r\n");
        ct.process(b"\r\n");
        ct.process(b"> \x1b[6n");
        assert_eq!(ct.position(), (7, 3));
    }

    #[test]
    fn test_cursor_tracker_csi_cursor_position() {
        let mut ct = CursorTracker::new(24, 80);
        ct.process(b"\x1b[5;10H");
        assert_eq!(ct.position(), (5, 10));
    }

    #[test]
    fn test_cursor_tracker_csi_cursor_home() {
        let mut ct = CursorTracker::new(24, 80);
        ct.process(b"some text\r\n\x1b[H");
        assert_eq!(ct.position(), (1, 1));
    }

    #[test]
    fn test_cursor_tracker_csi_relative_movement() {
        let mut ct = CursorTracker::new(24, 80);
        ct.process(b"\x1b[10;10H");
        ct.process(b"\x1b[3A");
        assert_eq!(ct.position(), (7, 10));
        ct.process(b"\x1b[2B");
        assert_eq!(ct.position(), (9, 10));
        ct.process(b"\x1b[5C");
        assert_eq!(ct.position(), (9, 15));
        ct.process(b"\x1b[3D");
        assert_eq!(ct.position(), (9, 12));
    }

    #[test]
    fn test_cursor_tracker_clear_screen() {
        let mut ct = CursorTracker::new(24, 80);
        ct.process(b"some text\r\n\x1b[2J");
        assert_eq!(ct.position(), (1, 1));
    }

    #[test]
    fn test_cursor_tracker_line_wrap() {
        let mut ct = CursorTracker::new(24, 10);
        ct.process(b"1234567890");
        assert_eq!(ct.position(), (2, 1));
    }

    #[test]
    fn test_cursor_tracker_scroll_at_bottom() {
        let mut ct = CursorTracker::new(3, 80);
        ct.process(b"line1\r\nline2\r\nline3\r\n");
        assert_eq!(ct.position(), (3, 1));
    }

    #[test]
    fn test_cursor_tracker_private_csi_ignored() {
        let mut ct = CursorTracker::new(24, 80);
        ct.process(b"\x1b[?1049h");
        assert_eq!(ct.position(), (1, 1));
    }

    #[test]
    fn test_cursor_tracker_set_size() {
        let mut ct = CursorTracker::new(24, 80);
        ct.process(b"\x1b[20;70H");
        assert_eq!(ct.position(), (20, 70));
        ct.set_size(10, 40);
        assert_eq!(ct.position(), (10, 40));
    }

    #[test]
    fn test_cursor_tracker_osc_skipped() {
        let mut ct = CursorTracker::new(24, 80);
        ct.process(b"\x1b]0;my title\x07hello");
        assert_eq!(ct.position(), (1, 6));
    }
}
