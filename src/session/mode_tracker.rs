/// Tracks DEC private terminal modes by scanning raw byte streams for
/// CSI ? Pm h/l sequences.  Handles sequences split across chunk boundaries
/// via a small state machine.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeSnapshot {
    pub app_cursor_keys: bool,
    pub bracketed_paste_mode: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Normal,
    Esc,
    Csi,
    CsiPrivate,
    CsiParam,
}

pub struct ModeTracker {
    app_cursor_keys: bool,
    bracketed_paste_mode: bool,
    state: State,
    param: u32,
}

impl ModeTracker {
    pub fn new() -> Self {
        Self {
            app_cursor_keys: false,
            bracketed_paste_mode: false,
            state: State::Normal,
            param: 0,
        }
    }

    pub fn snapshot(&self) -> ModeSnapshot {
        ModeSnapshot {
            app_cursor_keys: self.app_cursor_keys,
            bracketed_paste_mode: self.bracketed_paste_mode,
        }
    }

    /// Scan `data` for DEC private mode set/reset sequences.
    /// Returns `Some(snapshot)` if any tracked mode changed, `None` otherwise.
    pub fn process(&mut self, data: &[u8]) -> Option<ModeSnapshot> {
        let before = self.snapshot();

        for &b in data {
            match self.state {
                State::Normal => {
                    if b == 0x1b {
                        self.state = State::Esc;
                    }
                }
                State::Esc => {
                    if b == b'[' {
                        self.state = State::Csi;
                    } else {
                        // Not a CSI introducer — could be another ESC or
                        // a different escape type; restart.
                        self.state = if b == 0x1b { State::Esc } else { State::Normal };
                    }
                }
                State::Csi => {
                    if b == b'?' {
                        self.state = State::CsiPrivate;
                        self.param = 0;
                    } else {
                        // Not a DEC private sequence — skip.
                        self.state = if b == 0x1b { State::Esc } else { State::Normal };
                    }
                }
                State::CsiPrivate => {
                    if b.is_ascii_digit() {
                        self.param = self
                            .param
                            .saturating_mul(10)
                            .saturating_add((b - b'0') as u32);
                        self.state = State::CsiParam;
                    } else {
                        self.state = if b == 0x1b { State::Esc } else { State::Normal };
                    }
                }
                State::CsiParam => {
                    if b.is_ascii_digit() {
                        self.param = self
                            .param
                            .saturating_mul(10)
                            .saturating_add((b - b'0') as u32);
                    } else if b == b'h' {
                        self.apply(true);
                        self.state = State::Normal;
                    } else if b == b'l' {
                        self.apply(false);
                        self.state = State::Normal;
                    } else {
                        // Unexpected byte — reset.
                        self.state = if b == 0x1b { State::Esc } else { State::Normal };
                    }
                }
            }
        }

        let after = self.snapshot();
        if after != before { Some(after) } else { None }
    }

    fn apply(&mut self, enable: bool) {
        match self.param {
            1 => self.app_cursor_keys = enable,
            2004 => self.bracketed_paste_mode = enable,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_app_cursor_keys() {
        let mut t = ModeTracker::new();
        let snap = t.process(b"\x1b[?1h").unwrap();
        assert!(snap.app_cursor_keys);
        assert!(!snap.bracketed_paste_mode);
    }

    #[test]
    fn disable_app_cursor_keys() {
        let mut t = ModeTracker::new();
        t.process(b"\x1b[?1h");
        let snap = t.process(b"\x1b[?1l").unwrap();
        assert!(!snap.app_cursor_keys);
    }

    #[test]
    fn enable_bracketed_paste() {
        let mut t = ModeTracker::new();
        let snap = t.process(b"\x1b[?2004h").unwrap();
        assert!(snap.bracketed_paste_mode);
        assert!(!snap.app_cursor_keys);
    }

    #[test]
    fn disable_bracketed_paste() {
        let mut t = ModeTracker::new();
        t.process(b"\x1b[?2004h");
        let snap = t.process(b"\x1b[?2004l").unwrap();
        assert!(!snap.bracketed_paste_mode);
    }

    #[test]
    fn both_modes_in_one_chunk() {
        let mut t = ModeTracker::new();
        let snap = t.process(b"\x1b[?1h\x1b[?2004h").unwrap();
        assert!(snap.app_cursor_keys);
        assert!(snap.bracketed_paste_mode);
    }

    #[test]
    fn split_across_chunks() {
        let mut t = ModeTracker::new();
        // Split "\x1b[?2004h" between "200" and "4h"
        assert!(t.process(b"\x1b[?200").is_none());
        let snap = t.process(b"4h").unwrap();
        assert!(snap.bracketed_paste_mode);
    }

    #[test]
    fn split_at_esc() {
        let mut t = ModeTracker::new();
        assert!(t.process(b"\x1b").is_none());
        let snap = t.process(b"[?1h").unwrap();
        assert!(snap.app_cursor_keys);
    }

    #[test]
    fn split_at_csi() {
        let mut t = ModeTracker::new();
        assert!(t.process(b"\x1b[").is_none());
        let snap = t.process(b"?1h").unwrap();
        assert!(snap.app_cursor_keys);
    }

    #[test]
    fn no_change_returns_none() {
        let mut t = ModeTracker::new();
        assert!(t.process(b"\x1b[?1h").is_some());
        // Same enable again — no change.
        assert!(t.process(b"\x1b[?1h").is_none());
    }

    #[test]
    fn unknown_mode_ignored() {
        let mut t = ModeTracker::new();
        assert!(t.process(b"\x1b[?25h").is_none()); // DECTCEM
        assert!(!t.snapshot().app_cursor_keys);
        assert!(!t.snapshot().bracketed_paste_mode);
    }

    #[test]
    fn normal_text_no_trigger() {
        let mut t = ModeTracker::new();
        assert!(t.process(b"hello world\r\n").is_none());
        assert!(t.process(b"ls -la\n").is_none());
    }

    #[test]
    fn embedded_in_output() {
        let mut t = ModeTracker::new();
        let snap = t.process(b"some output\x1b[?1hmore text").unwrap();
        assert!(snap.app_cursor_keys);
    }

    #[test]
    fn interrupted_sequence_resets() {
        let mut t = ModeTracker::new();
        // Interrupted CSI — the 'X' aborts, then a fresh sequence follows.
        let snap = t.process(b"\x1b[?X\x1b[?1h").unwrap();
        assert!(snap.app_cursor_keys);
    }

    #[test]
    fn snapshot_reflects_current_state() {
        let mut t = ModeTracker::new();
        let s = t.snapshot();
        assert!(!s.app_cursor_keys);
        assert!(!s.bracketed_paste_mode);

        t.process(b"\x1b[?1h\x1b[?2004h");
        let s = t.snapshot();
        assert!(s.app_cursor_keys);
        assert!(s.bracketed_paste_mode);
    }
}
