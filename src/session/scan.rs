//! Single-pass pseudo-terminal output scanner.
//!
//! The daemon has to do four things with every byte the child writes:
//!
//! 1. answer the terminal-capability probes that need a session-global reply,
//! 2. strip terminal↔application protocol traffic (device reports, capability
//!    probes, OSC chatter) that must never reach an attached terminal,
//! 3. remember the one-way notifications (title, progress, cursor shape) the
//!    screen parser does not model, and
//! 4. measure how much of the output represents real screen activity.
//!
//! The previous implementation ran roughly twenty independent
//! `regex::bytes::Regex::replace_all` passes plus a second full scan for query
//! extraction, allocating a fresh buffer per pass. That capped the reader
//! thread at ~35 MB/s, which is what made large pastes crawl: every echoed
//! byte of a 1 MB paste went through the whole cascade.
//!
//! [`PtyScanner`] replaces all of it with one byte-level VT state machine that
//! walks the chunk exactly once, copies runs of plain text in bulk, and never
//! allocates on the steady-state path.
//!
//! # ConPTY bare sequences
//!
//! Windows ConPTY sometimes drops the introducing `ESC` of a sequence, so a
//! cursor report can arrive as `[35;1R` and a title as `]0;title\x07`. Those
//! "bare" forms are recognised only on Windows: on Unix the same bytes are
//! ordinary program output, and treating them as escape sequences corrupts the
//! rendering of anything that legitimately prints `[12;3R`.

use memchr::{memchr, memchr3};

use super::pty::{TerminalQuery, TerminalSignals};

/// Upper bound on the incomplete-sequence prefix carried between chunks.
///
/// A byte run that merely *looks* like the start of a sequence (plain text
/// ending in `]12;`, say) would otherwise be buffered forever, because every
/// following chunk keeps extending the same unterminated candidate. That
/// stalls output for every attached client and grows the buffer without bound.
/// Real sequences are far shorter, so a candidate past this limit is flushed
/// verbatim instead.
pub(crate) const MAX_PENDING_ESCAPE_BYTES: usize = 4096;

/// Whether escape-less ConPTY sequence variants are recognised.
const BARE_CONPTY_FORMS: bool = cfg!(windows);

/// Everything one `scan` call produced.
///
/// The reader thread keeps a single instance alive and reuses its buffers, so
/// steady-state scanning performs no allocation at all.
#[derive(Debug, Default)]
pub struct ScanOut {
    /// The canonical filtered stream: what gets rendered, retained, persisted
    /// and broadcast.
    pub filtered: Vec<u8>,
    /// Capability probes that need a daemon-generated reply written back to
    /// the child's standard input.
    pub queries: Vec<TerminalQuery>,
    /// Bytes of `filtered` that only carry one-way terminal signals:
    /// window/icon titles (`OSC 0/1/2`) and progress/busy notifications
    /// (`OSC 9;4`). They are forwarded to clients but do not count as screen
    /// activity, so a child that only retitles itself (e.g.
    /// `]0;[ ! ] Action Required`) or animates a progress indicator is still
    /// considered idle. This is a deliberate trade-off: a bare title flip
    /// usually accompanies an input-required prompt rather than real
    /// progress, and genuine progress almost always comes with regular
    /// screen output that still counts.
    pub signal_bytes: usize,
}

impl ScanOut {
    /// Bytes of this chunk that changed what the user can see.
    pub fn meaningful_bytes(&self) -> usize {
        self.filtered.len().saturating_sub(self.signal_bytes)
    }

    fn reset(&mut self) {
        self.filtered.clear();
        self.queries.clear();
        self.signal_bytes = 0;
    }
}

/// Byte-level pseudo-terminal output scanner.
///
/// One instance lives per session, owned by the PTY reader thread. It carries
/// the partial-sequence prefix and the retained terminal signals across chunk
/// boundaries, so sequences split at any byte are still handled correctly.
pub struct PtyScanner {
    /// Trailing bytes of the previous chunk that form an incomplete sequence.
    pending: Vec<u8>,
    /// Scratch buffer for `pending` + current chunk, reused between calls.
    joined: Vec<u8>,
    /// One-way notifications the session has most recently emitted.
    signals: TerminalSignals,
    /// Set when `signals` changed since the last time it was published.
    signals_dirty: bool,
}

impl PtyScanner {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            joined: Vec::new(),
            signals: TerminalSignals::default(),
            signals_dirty: false,
        }
    }

    /// The retained one-way notifications, if they changed since the last call.
    ///
    /// Returning `None` for an unchanged snapshot lets the reader thread skip
    /// the session write lock for the overwhelming majority of chunks.
    pub fn take_changed_signals(&mut self) -> Option<TerminalSignals> {
        if !self.signals_dirty {
            return None;
        }
        self.signals_dirty = false;
        Some(self.signals.clone())
    }

    /// Scan one raw chunk, filling `out` with the filtered stream and the
    /// probes that need a reply.
    pub fn scan(&mut self, data: &[u8], out: &mut ScanOut) {
        out.reset();

        if self.pending.is_empty() {
            out.filtered.reserve(data.len());
            self.scan_slice(data, out);
            return;
        }

        // A sequence straddled the previous read boundary. Rejoin so the state
        // machine sees it whole; this is rare enough that the copy is free
        // compared to getting the split case wrong.
        let mut joined = std::mem::take(&mut self.joined);
        joined.clear();
        joined.reserve(self.pending.len() + data.len());
        joined.extend_from_slice(&self.pending);
        joined.extend_from_slice(data);
        self.pending.clear();
        out.filtered.reserve(joined.len());
        self.scan_slice(&joined, out);
        self.joined = joined;
    }

    fn scan_slice(&mut self, data: &[u8], out: &mut ScanOut) {
        let mut index = 0usize;

        while index < data.len() {
            // Bulk-copy the plain-text run up to the next byte that could
            // introduce a sequence. This is where almost all bytes go.
            let next = if BARE_CONPTY_FORMS {
                memchr3(0x1b, b'[', b']', &data[index..])
            } else {
                memchr(0x1b, &data[index..])
            };
            match next {
                None => {
                    out.filtered.extend_from_slice(&data[index..]);
                    return;
                }
                Some(0) => {}
                Some(offset) => {
                    out.filtered.extend_from_slice(&data[index..index + offset]);
                    index += offset;
                }
            }

            let consumed = match data[index] {
                0x1b => self.escape(data, index, out),
                b'[' => Some(self.bare_csi(data, index, out)),
                _ => self.bare_osc(data, index, out),
            };

            match consumed {
                Some(len) => index += len,
                None => {
                    let rest = &data[index..];
                    if rest.len() >= MAX_PENDING_ESCAPE_BYTES {
                        // Not a real sequence — it would have terminated long
                        // ago. Release it rather than stalling the stream.
                        out.filtered.extend_from_slice(rest);
                    } else {
                        self.pending.extend_from_slice(rest);
                    }
                    return;
                }
            }
        }
    }

    /// Handle an `ESC`-introduced sequence starting at `start`.
    fn escape(&mut self, data: &[u8], start: usize, out: &mut ScanOut) -> Option<usize> {
        let introducer = *data.get(start + 1)?;
        match introducer {
            b'[' => self.csi(data, start, start + 2, out),
            b']' => self.osc(data, start, start + 2, true, out),
            // Device Control String, Application Program Command, Privacy
            // Message and Start Of String all carry terminal↔application
            // payloads (XTVERSION replies, kitty graphics, …) that are never
            // renderable content.
            b'P' | b'_' | b'^' | b'X' => string_sequence(data, start, start + 2),
            _ => {
                // Two-byte escapes (charset selection, DECSC, …) pass through.
                // Anything longer resumes in the ground state, which copies the
                // remaining printable bytes verbatim.
                out.filtered.extend_from_slice(&data[start..start + 2]);
                Some(2)
            }
        }
    }

    /// Parse `CSI <params> <intermediates> <final>` and decide its fate.
    fn csi(
        &mut self,
        data: &[u8],
        start: usize,
        params_start: usize,
        out: &mut ScanOut,
    ) -> Option<usize> {
        let mut cursor = params_start;
        while matches!(data.get(cursor), Some(0x30..=0x3f)) {
            cursor += 1;
        }
        let params_end = cursor;
        while matches!(data.get(cursor), Some(0x20..=0x2f)) {
            cursor += 1;
        }
        let intermediates_end = cursor;

        let final_byte = *data.get(cursor)?;
        if !(0x40..=0x7e).contains(&final_byte) {
            // Malformed: emit the introducer and let the ground state re-read
            // the parameter bytes as the plain text they evidently are.
            out.filtered.extend_from_slice(&data[start..params_start]);
            return Some(params_start - start);
        }

        let end = cursor + 1;
        let params = &data[params_start..params_end];
        let intermediates = &data[params_end..intermediates_end];

        match classify_csi(params, intermediates, final_byte) {
            CsiAction::Emit => {
                if final_byte == b'q' && intermediates == b" " {
                    self.record_cursor_style(params);
                }
                out.filtered.extend_from_slice(&data[start..end]);
            }
            CsiAction::Strip => {}
            CsiAction::StripAndAnswer(query) => out.queries.push(query),
        }

        Some(end - start)
    }

    /// Parse `OSC <ps> ; <payload> <terminator>` and decide its fate.
    ///
    /// `escaped` distinguishes the well-formed `ESC ]` introducer from the bare
    /// ConPTY variant, which only ever ends in `BEL`: a lone backslash is far
    /// more likely to be payload (Windows shells report titles such as
    /// `C:\Users\me`) than a mangled string terminator.
    fn osc(
        &mut self,
        data: &[u8],
        start: usize,
        ps_start: usize,
        escaped: bool,
        out: &mut ScanOut,
    ) -> Option<usize> {
        let introducer_len = ps_start - start;
        let mut cursor = ps_start;
        while matches!(data.get(cursor), Some(byte) if byte.is_ascii_digit()) {
            cursor += 1;
        }
        let ps_end = cursor;
        let ps_len = ps_end - ps_start;
        if ps_len == 0 || ps_len > 3 {
            if cursor >= data.len() {
                return None;
            }
            out.filtered.extend_from_slice(&data[start..ps_start]);
            return Some(introducer_len);
        }

        let payload_start = if data.get(cursor) == Some(&b';') {
            cursor += 1;
            cursor
        } else {
            cursor
        };

        // Locate the string terminator.
        let (payload_end, end) = loop {
            match data.get(cursor) {
                None => return None,
                Some(0x07) => break (cursor, cursor + 1),
                Some(0x1b) => {
                    if cursor + 1 >= data.len() {
                        return None;
                    }
                    if escaped && data[cursor + 1] == b'\\' {
                        break (cursor, cursor + 2);
                    }
                    // An embedded escape means this was never an OSC. Emit the
                    // introducer and rescan from the parameter bytes.
                    out.filtered.extend_from_slice(&data[start..ps_start]);
                    return Some(introducer_len);
                }
                Some(_) => cursor += 1,
            }
        };

        let ps = &data[ps_start..ps_end];
        let payload = &data[payload_start..payload_end];

        if payload.first() == Some(&b'?') {
            match ps {
                b"10" => out.queries.push(TerminalQuery::ForegroundColor),
                b"11" => out.queries.push(TerminalQuery::BackgroundColor),
                _ => {}
            }
            return Some(end - start);
        }

        if payload_start == ps_end || !is_passthrough_osc(ps, payload) {
            return Some(end - start);
        }

        self.record_osc(ps, payload);

        // Re-introduce the escape ConPTY dropped: forwarding the bare form
        // verbatim is not a valid sequence, so the client's terminal would
        // print `]0;title` as literal text.
        let emitted_start = out.filtered.len();
        out.filtered.extend_from_slice(b"\x1b]");
        out.filtered.extend_from_slice(&data[ps_start..payload_end]);
        out.filtered.extend_from_slice(&data[payload_end..end]);
        // Only passthrough OSC sequences reach this point, and all of them
        // (titles, progress/busy) are one-way terminal signals rather than
        // screen activity.
        out.signal_bytes += out.filtered.len() - emitted_start;

        Some(end - start)
    }

    /// Recognise the escape-less ConPTY cursor report `[<row>;<col>R` and
    /// status probes `[5n` / `[6n`.
    ///
    /// Unlike the OSC variant these are never buffered across chunks: a
    /// truncated candidate is far more likely to be ordinary text ending in
    /// `[12`, and holding that back until the child writes again would leave
    /// it stuck on screen for as long as the child stays idle.
    fn bare_csi(&mut self, data: &[u8], start: usize, out: &mut ScanOut) -> usize {
        let emit_literal = |out: &mut ScanOut| {
            out.filtered.push(b'[');
            1usize
        };

        let mut cursor = start + 1;
        if data.get(cursor) == Some(&b'?') {
            cursor += 1;
        }

        let digits_start = cursor;
        while matches!(data.get(cursor), Some(byte) if byte.is_ascii_digit()) {
            cursor += 1;
        }
        if cursor == digits_start {
            return emit_literal(out);
        }

        // `[5n` / `[6n`
        if data.get(cursor) == Some(&b'n') {
            let digits = &data[digits_start..cursor];
            if digits == b"5" || digits == b"6" {
                return cursor + 1 - start;
            }
            return emit_literal(out);
        }

        // `[<row>;<col>R`
        if data.get(cursor) != Some(&b';') {
            return emit_literal(out);
        }
        cursor += 1;
        let second_start = cursor;
        while matches!(data.get(cursor), Some(byte) if byte.is_ascii_digit()) {
            cursor += 1;
        }
        if cursor == second_start || data.get(cursor) != Some(&b'R') {
            return emit_literal(out);
        }

        cursor + 1 - start
    }

    fn bare_osc(&mut self, data: &[u8], start: usize, out: &mut ScanOut) -> Option<usize> {
        self.osc(data, start, start + 1, false, out)
    }

    fn record_cursor_style(&mut self, params: &[u8]) {
        // An empty or zero parameter restores the terminal default, which needs
        // no replay.
        let next = if params.is_empty() || params == b"0" {
            None
        } else {
            Some(params.to_vec())
        };
        if self.signals.set_cursor_style(next) {
            self.signals_dirty = true;
        }
    }

    fn record_osc(&mut self, ps: &[u8], payload: &[u8]) {
        if self.signals.record_osc(ps, payload) {
            self.signals_dirty = true;
        }
    }
}

impl Default for PtyScanner {
    fn default() -> Self {
        Self::new()
    }
}

/// Consume a `DCS` / `APC` / `PM` / `SOS` string sequence up to its terminator.
fn string_sequence(data: &[u8], start: usize, payload_start: usize) -> Option<usize> {
    let mut cursor = payload_start;
    loop {
        match data.get(cursor) {
            None => return None,
            Some(0x07) => return Some(cursor + 1 - start),
            Some(0x1b) => {
                if cursor + 1 >= data.len() {
                    return None;
                }
                if data[cursor + 1] == b'\\' {
                    return Some(cursor + 2 - start);
                }
                cursor += 1;
            }
            Some(_) => cursor += 1,
        }
    }
}

enum CsiAction {
    /// Renderable content: forward verbatim.
    Emit,
    /// Terminal↔application protocol traffic: drop it.
    Strip,
    /// A probe the daemon answers on the session's behalf.
    StripAndAnswer(TerminalQuery),
}

/// Decide what to do with one parsed control sequence.
///
/// Everything that is part of the terminal↔application *protocol* — device
/// reports, capability probes and their replies — is dropped: a detached
/// session has no terminal to answer them, and an attached client must not let
/// its own terminal answer on the child's behalf, because the reply would be
/// injected into the child's standard input out of band.
fn classify_csi(params: &[u8], intermediates: &[u8], final_byte: u8) -> CsiAction {
    let private = match params.first() {
        Some(&byte @ 0x3c..=0x3f) => byte,
        _ => 0,
    };
    let digits = if private == 0 { params } else { &params[1..] };

    match final_byte {
        // Cursor Position Report: `CSI <row> ; <col> R`.
        b'R' if (private == 0 || private == b'?') && is_two_numeric_params(digits) => {
            CsiAction::Strip
        }
        // Device Status Report probes and replies.
        b'n' if private == b'?' => CsiAction::Strip,
        b'n' if private == 0 && !digits.is_empty() && digits.iter().all(u8::is_ascii_digit) => {
            match digits {
                b"6" => CsiAction::StripAndAnswer(TerminalQuery::CursorPositionReport),
                b"5" => CsiAction::StripAndAnswer(TerminalQuery::DeviceStatusReport),
                _ => CsiAction::Strip,
            }
        }
        // Primary and secondary Device Attributes, probe and reply alike.
        b'c' => CsiAction::Strip,
        // DEC private mode report: `CSI ? <mode> $ p`.
        b'p' if private == b'?' && intermediates == b"$" => CsiAction::Strip,
        // XTVERSION: `CSI > <n> q`. Plain `CSI <n> SP q` is DECSCUSR and stays.
        b'q' if private == b'>' => CsiAction::Strip,
        // Kitty keyboard protocol probe and reply. The `>`/`<`/`=` forms are
        // application commands to the terminal and must survive.
        b'u' if private == b'?' => CsiAction::Strip,
        // Window-size-in-pixels probes: `CSI 14 t` … `CSI 19 t`.
        b't' if private == 0 && matches!(leading_number(digits), Some(14..=19)) => CsiAction::Strip,
        _ => CsiAction::Emit,
    }
}

fn is_two_numeric_params(digits: &[u8]) -> bool {
    let mut parts = digits.split(|&byte| byte == b';');
    let (Some(first), Some(second), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !first.is_empty()
        && !second.is_empty()
        && first.iter().all(u8::is_ascii_digit)
        && second.iter().all(u8::is_ascii_digit)
}

fn leading_number(digits: &[u8]) -> Option<u32> {
    let first = digits.split(|&byte| byte == b';').next()?;
    if first.is_empty() || !first.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(first).ok()?.parse().ok()
}

/// Whether an Operating System Command carries a one-way semantic notification
/// that attached terminals should still observe.
///
/// OSC 0/1/2 set the icon and window title; OSC 9;4 drives the progress/busy
/// indicator. Everything else (shell integration, working-directory reports,
/// clipboard access, hyperlinks, colour queries) is either terminal↔application
/// protocol or host-specific state that must not follow the session onto
/// another user's terminal.
pub(crate) fn is_passthrough_osc(ps: &[u8], payload: &[u8]) -> bool {
    matches!(ps, b"0" | b"1" | b"2") || (ps == b"9" && payload.starts_with(b"4;"))
}

/// Extract the passthrough Operating System Commands from a canonical
/// filtered stream.
///
/// The Windows attach client repaints from the terminal parser's canonical
/// screen state, which models neither window titles nor progress indicators;
/// the sequences carrying those notifications are re-emitted alongside each
/// repaint so they survive. Streams reaching the client were already
/// normalised by the daemon's scanner — bare ConPTY forms regained their
/// `ESC` introducer — so only the well-formed `ESC ]` form is recognised
/// here.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn extract_passthrough_osc_sequences(data: &[u8]) -> Vec<u8> {
    let mut forwarded = Vec::new();
    let mut index = 0usize;

    while let Some(offset) = memchr(0x1b, &data[index..]) {
        let start = index + offset;
        index = start + 1;
        if data.get(start + 1) != Some(&b']') {
            continue;
        }

        let ps_start = start + 2;
        let mut cursor = ps_start;
        while matches!(data.get(cursor), Some(byte) if byte.is_ascii_digit()) {
            cursor += 1;
        }
        if cursor == ps_start || data.get(cursor) != Some(&b';') {
            continue;
        }
        let ps = &data[ps_start..cursor];
        let payload_start = cursor + 1;
        cursor = payload_start;

        let mut bounds: Option<(usize, usize)> = None;
        loop {
            match data.get(cursor) {
                None => break,
                Some(0x07) => {
                    bounds = Some((cursor, cursor + 1));
                    break;
                }
                Some(0x1b) => {
                    if data.get(cursor + 1) == Some(&b'\\') {
                        bounds = Some((cursor, cursor + 2));
                    }
                    break;
                }
                Some(_) => cursor += 1,
            }
        }
        let Some((payload_end, end)) = bounds else {
            continue;
        };

        if is_passthrough_osc(ps, &data[payload_start..payload_end]) {
            forwarded.extend_from_slice(&data[start..end]);
        }
        index = end;
    }

    forwarded
}

/// Parameters of the last cursor-style sequence (`CSI <n> SP q`, DECSCUSR) in
/// a canonical filtered stream, or `None` when it carries no cursor-shape
/// change. The terminal parser models neither shape nor blink, so the Windows
/// attach client re-emits the last one alongside each repaint.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn last_cursor_style_params(data: &[u8]) -> Option<&[u8]> {
    let mut found: Option<&[u8]> = None;
    let mut index = 0usize;

    while let Some(offset) = memchr(0x1b, &data[index..]) {
        let start = index + offset;
        if data.get(start + 1) != Some(&b'[') {
            index = start + 1;
            continue;
        }

        let params_start = start + 2;
        let mut cursor = params_start;
        while matches!(data.get(cursor), Some(byte) if byte.is_ascii_digit()) {
            cursor += 1;
        }

        if data.get(cursor) == Some(&b' ') && data.get(cursor + 1) == Some(&b'q') {
            found = Some(&data[params_start..cursor]);
            index = cursor + 2;
        } else {
            index = params_start;
        }
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test shim mirroring the reader thread's use of the scanner.
    struct Harness {
        scanner: PtyScanner,
        out: ScanOut,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                scanner: PtyScanner::new(),
                out: ScanOut::default(),
            }
        }

        fn filter(&mut self, chunk: &[u8]) -> Vec<u8> {
            self.scanner.scan(chunk, &mut self.out);
            self.out.filtered.clone()
        }

        fn filter_text(&mut self, chunk: &str) -> String {
            String::from_utf8(self.filter(chunk.as_bytes()))
                .expect("test chunk should remain valid UTF-8 after filtering")
        }

        fn pending(&self) -> &str {
            std::str::from_utf8(&self.scanner.pending)
                .expect("test pending bytes should remain valid UTF-8")
        }

        fn queries(&self) -> &[TerminalQuery] {
            &self.out.queries
        }
    }

    // -----------------------------------------------------------------------
    // Plain content is never touched
    // -----------------------------------------------------------------------

    #[test]
    fn plain_text_passes_through_unchanged() {
        let mut harness = Harness::new();
        let text = "Hello, world! 123 foo bar\nline two\ttab";
        assert_eq!(harness.filter_text(text), text);
        assert!(harness.pending().is_empty());
    }

    #[test]
    fn sgr_colour_sequences_are_preserved() {
        let mut harness = Harness::new();
        let text = "\x1b[38;5;196mred\x1b[0m \x1b[48;2;0;128;255mblue bg\x1b[0m";
        assert_eq!(harness.filter_text(text), text);
    }

    #[test]
    fn invalid_utf8_bytes_are_preserved() {
        let mut harness = Harness::new();
        assert_eq!(
            harness.filter(b"before\x80\x1b[6nafter\xff"),
            b"before\x80after\xff"
        );
    }

    #[test]
    fn invalid_utf8_bytes_are_preserved_across_a_split_query() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter(b"before\x80\x1b[6"), b"before\x80");
        assert_eq!(harness.filter(b"nafter\xff"), b"after\xff");
    }

    #[test]
    fn restore_cursor_csi_u_is_preserved() {
        // `CSI u` restores the cursor; only `CSI ? u` is the kitty probe.
        let mut harness = Harness::new();
        assert_eq!(
            harness.filter_text("before\x1b[uafter"),
            "before\x1b[uafter"
        );
    }

    #[test]
    fn kitty_keyboard_stack_commands_are_preserved() {
        // `CSI > flags u` pushes keyboard flags — an application command to the
        // terminal, not a capability probe.
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("\x1b[>1u\x1b[<u"), "\x1b[>1u\x1b[<u");
    }

    #[test]
    fn window_manipulation_other_than_size_probes_is_preserved() {
        let mut harness = Harness::new();
        assert_eq!(
            harness.filter_text("\x1b[22;0t\x1b[23;0t"),
            "\x1b[22;0t\x1b[23;0t"
        );
    }

    #[test]
    fn decscusr_cursor_shape_is_preserved() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("\x1b[6 q"), "\x1b[6 q");
    }

    #[test]
    fn two_byte_escapes_are_preserved() {
        let mut harness = Harness::new();
        assert_eq!(
            harness.filter_text("\x1b(B\x1b7text\x1b8"),
            "\x1b(B\x1b7text\x1b8"
        );
    }

    // -----------------------------------------------------------------------
    // Protocol traffic is stripped
    // -----------------------------------------------------------------------

    #[test]
    fn cursor_position_reports_are_stripped() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("a\x1b[35;1Rb\x1b[?35;1Rc"), "abc");
    }

    #[test]
    fn status_report_probes_and_replies_are_stripped() {
        let mut harness = Harness::new();
        assert_eq!(
            harness.filter_text("hello\x1b[6nworld\x1b[5n!"),
            "helloworld!"
        );
        assert_eq!(harness.filter_text("before\x1b[0nafter"), "beforeafter");
        assert_eq!(harness.filter_text("before\x1b[?996nafter"), "beforeafter");
    }

    #[test]
    fn device_attribute_probes_and_replies_are_stripped() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("before\x1b[cafter"), "beforeafter");
        assert_eq!(harness.filter_text("before\x1b[>cafter"), "beforeafter");
        assert_eq!(
            harness.filter_text("before\x1b[?62;1;2;6;22cafter"),
            "beforeafter"
        );
        assert_eq!(
            harness.filter_text("before\x1b[>1;0;0cafter"),
            "beforeafter"
        );
    }

    #[test]
    fn dec_private_mode_reports_are_stripped() {
        let mut harness = Harness::new();
        assert_eq!(
            harness.filter_text("before\x1b[?2004$pafter"),
            "beforeafter"
        );
        assert_eq!(
            harness.filter_text("\x1b[?1016$p\x1b[?2027$p\x1b[?2004$pvisible"),
            "visible"
        );
    }

    #[test]
    fn xtversion_and_kitty_probes_are_stripped() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("before\x1b[>0qafter"), "beforeafter");
        assert_eq!(harness.filter_text("before\x1b[?uafter"), "beforeafter");
    }

    #[test]
    fn window_size_probes_are_stripped() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("before\x1b[14tafter"), "beforeafter");
        assert_eq!(harness.filter_text("before\x1b[18tafter"), "beforeafter");
    }

    #[test]
    fn string_sequences_are_stripped() {
        let mut harness = Harness::new();
        assert_eq!(
            harness.filter_text("before\x1b_Gi=31337;OK\x1b\\after"),
            "beforeafter"
        );
        assert_eq!(
            harness.filter_text("before\x1b_Gi=31337,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\after"),
            "beforeafter"
        );
        assert_eq!(
            harness.filter_text("before\x1bP>|xterm 388\x1b\\after"),
            "beforeafter"
        );
    }

    #[test]
    fn back_to_back_probes_leave_nothing_behind() {
        let mut harness = Harness::new();
        let text = "\x1b[6n\x1b[5n\x1b[c\x1b[>c\x1b[>0q\x1b[?u\x1b[14t";
        assert_eq!(harness.filter_text(text), "");
        assert!(harness.pending().is_empty());
    }

    #[test]
    fn startup_probe_burst_keeps_only_renderable_bytes() {
        let mut harness = Harness::new();
        // The burst bubbletea-based TUIs emit at startup.
        let result = harness.filter_text(
            "\x1b[>0q\x1b[?25l\x1b[s\x1b[?1016$p\x1b[?2027$p\x1b[?2031$p\x1b[?1004$p\
             \x1b[?2004$p\x1b[?2026$p\x1b[?u\x1b[H\x1b[?1049hTUI_CONTENT",
        );
        assert_eq!(result, "\x1b[?25l\x1b[s\x1b[H\x1b[?1049hTUI_CONTENT");
    }

    // -----------------------------------------------------------------------
    // Operating System Commands
    // -----------------------------------------------------------------------

    #[test]
    fn title_and_progress_notifications_pass_through() {
        let mut harness = Harness::new();
        let text = "before\x1b]0;relay build\x07mid\x1b]9;4;3;0\x07after";
        assert_eq!(harness.filter_text(text), text);
    }

    #[test]
    fn progress_notifications_do_not_count_as_screen_activity() {
        let mut harness = Harness::new();
        harness.filter(b"\x1b]9;4;3;0\x07");
        assert_eq!(harness.out.meaningful_bytes(), 0);

        harness.filter(b"x\x1b]9;4;3;0\x07");
        assert_eq!(harness.out.meaningful_bytes(), 1);
    }

    #[test]
    fn title_notifications_do_not_count_as_screen_activity() {
        // A bare title change (e.g. `]0;[ ! ] Action Required | build`) is
        // how most agent CLIs flag that they are waiting for input; counting
        // it as activity would keep resetting the silence clock and suppress
        // the very notification it announces.
        let mut harness = Harness::new();
        let text = b"\x1b]0;relay\x07";
        harness.filter(text);
        assert_eq!(harness.out.meaningful_bytes(), 0);

        // Titles sharing a chunk with real output only discount themselves.
        harness.filter(b"x\x1b]0;relay\x07");
        assert_eq!(harness.out.meaningful_bytes(), 1);
    }

    #[test]
    fn other_operating_system_commands_are_stripped() {
        let mut harness = Harness::new();
        assert_eq!(
            harness.filter_text("before\x1b]7;file://host/home/me/project\x07after"),
            "beforeafter"
        );
        assert_eq!(
            harness.filter_text("a\x1b]8;;https://example.com\x1b\\b"),
            "ab"
        );
        assert_eq!(harness.filter_text("a\x1b]9;plain notification\x07b"), "ab");
    }

    #[test]
    fn colour_replies_are_stripped() {
        let mut harness = Harness::new();
        assert_eq!(
            harness
                .filter_text("a\x1b]10;rgb:ffff/ffff/ffff\x07b\x1b]11;rgb:0000/0000/0000\x1b\\c"),
            "abc"
        );
    }

    #[test]
    fn string_terminator_style_is_preserved_for_passthrough_sequences() {
        let mut harness = Harness::new();
        assert_eq!(
            harness.filter_text("\x1b]2;relay\x1b\\"),
            "\x1b]2;relay\x1b\\"
        );
    }

    #[test]
    fn backslashes_inside_a_title_stay_payload() {
        let mut harness = Harness::new();
        let text = "before\x1b]0;C:\\Users\\me\x07after";
        assert_eq!(harness.filter_text(text), text);
    }

    #[test]
    fn operating_system_command_without_payload_is_stripped() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("a\x1b]0\x07b"), "ab");
    }

    // -----------------------------------------------------------------------
    // Cross-chunk splits
    // -----------------------------------------------------------------------

    #[test]
    fn split_status_probe_is_stripped() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("hello\x1b[6"), "hello");
        assert_eq!(harness.pending(), "\x1b[6");
        assert_eq!(harness.filter_text("nworld"), "world");
        assert!(harness.pending().is_empty());
    }

    #[test]
    fn split_dec_private_mode_report_is_stripped() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("hello\x1b[?2004$"), "hello");
        assert_eq!(harness.filter_text("pworld"), "world");
    }

    #[test]
    fn split_title_notification_is_reassembled() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("before\x1b]0;relay"), "before");
        assert_eq!(harness.pending(), "\x1b]0;relay");
        assert_eq!(
            harness.filter_text(" busy\x07after"),
            "\x1b]0;relay busy\x07after"
        );
        assert!(harness.pending().is_empty());
    }

    #[test]
    fn split_title_containing_backslash_is_reassembled() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("before\x1b]0;C:\\Users"), "before");
        assert_eq!(
            harness.filter_text("\\me\x07after"),
            "\x1b]0;C:\\Users\\me\x07after"
        );
    }

    #[test]
    fn split_stripped_operating_system_command_is_reassembled() {
        let mut harness = Harness::new();
        assert_eq!(
            harness.filter_text("before\x1b]7;file://host/home/me/project/target/debug"),
            "before"
        );
        assert_eq!(harness.filter_text("\x07after"), "after");
        assert!(harness.pending().is_empty());
    }

    #[test]
    fn split_string_sequences_are_stripped() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("hello\x1b_Gi=31337"), "hello");
        assert_eq!(harness.filter_text(";OK\x1b\\world"), "world");

        assert_eq!(harness.filter_text("hello\x1bP>|xterm"), "hello");
        assert_eq!(harness.filter_text(" 388\x1b\\world"), "world");
    }

    #[test]
    fn lone_escape_at_the_boundary_is_carried_over() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("text\x1b"), "text");
        assert_eq!(harness.pending(), "\x1b");
        assert_eq!(harness.filter_text("[0mmore"), "\x1b[0mmore");
    }

    #[test]
    fn oversized_partial_sequence_is_flushed_verbatim() {
        // Plain text that merely looks like the start of a sequence must not
        // stall the stream by growing `pending` forever.
        let mut harness = Harness::new();
        let mut text = b"\x1b]0;".to_vec();
        text.extend(std::iter::repeat_n(b'x', MAX_PENDING_ESCAPE_BYTES));

        assert_eq!(harness.filter(&text), text);
        assert!(harness.pending().is_empty());
    }

    #[test]
    fn empty_chunk_keeps_pending_intact() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("\x1b["), "");
        assert_eq!(harness.pending(), "\x1b[");
        assert_eq!(harness.filter_text(""), "");
        assert_eq!(harness.pending(), "\x1b[");
        assert_eq!(harness.filter_text("6n"), "");
        assert!(harness.pending().is_empty());
    }

    #[test]
    fn chunking_does_not_change_the_result() {
        let source: &[u8] = b"\x1b]0;title\x07plain \x1b[38;5;9mred\x1b[0m \x1b[6n\
            \x1b[12;34R\x1b]7;cwd\x07\x1bP>|x\x1b\\ tail\x1b[?2004$p done";

        let mut whole = Harness::new();
        let expected = whole.filter(source);

        for size in 1..=source.len() {
            let mut harness = Harness::new();
            let mut actual = Vec::new();
            for chunk in source.chunks(size) {
                actual.extend_from_slice(&harness.filter(chunk));
            }
            assert_eq!(
                actual, expected,
                "chunk size {size} changed the filtered stream"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Query answering
    // -----------------------------------------------------------------------

    #[test]
    fn cursor_position_probe_is_reported() {
        let mut harness = Harness::new();
        harness.filter(b"\x1b[6n");
        assert_eq!(harness.queries(), &[TerminalQuery::CursorPositionReport]);
    }

    #[test]
    fn status_probe_is_reported() {
        let mut harness = Harness::new();
        harness.filter(b"\x1b[5n");
        assert_eq!(harness.queries(), &[TerminalQuery::DeviceStatusReport]);
    }

    #[test]
    fn colour_probes_are_reported() {
        let mut harness = Harness::new();
        harness.filter(b"\x1b]10;?\x07\x1b]11;?\x1b\\");
        assert_eq!(
            harness.queries(),
            &[
                TerminalQuery::ForegroundColor,
                TerminalQuery::BackgroundColor
            ]
        );
    }

    #[test]
    fn capability_probes_are_not_answered_by_the_daemon() {
        // Device attributes, XTVERSION, DECRPM and kitty probes are stripped so
        // they cannot reach a client's terminal, but answering them on the
        // session's behalf would inject bytes the child never asked this
        // terminal for.
        let mut harness = Harness::new();
        harness.filter(b"\x1b[c\x1b[>c\x1b[>0q\x1b[?2004$p\x1b[?u");
        assert!(harness.queries().is_empty());
    }

    #[test]
    fn split_probe_is_reported_once_complete() {
        let mut harness = Harness::new();
        harness.filter(b"hello\x1b[6");
        assert!(harness.queries().is_empty());
        harness.filter(b"nworld");
        assert_eq!(harness.queries(), &[TerminalQuery::CursorPositionReport]);
    }

    // -----------------------------------------------------------------------
    // Retained terminal signals
    // -----------------------------------------------------------------------

    #[test]
    fn signals_are_published_only_when_they_change() {
        let mut harness = Harness::new();
        assert!(harness.scanner.take_changed_signals().is_none());

        harness.filter(b"\x1b]0;relay build\x07");
        let signals = harness
            .scanner
            .take_changed_signals()
            .expect("a new title changes the retained signals");
        assert_eq!(signals.restore_bytes(), b"\x1b]0;relay build\x07".to_vec());
        assert!(harness.scanner.take_changed_signals().is_none());

        harness.filter(b"\x1b]0;relay build\x07");
        assert!(
            harness.scanner.take_changed_signals().is_none(),
            "an identical title must not wake the session write lock"
        );
    }

    #[test]
    fn cursor_style_is_retained() {
        let mut harness = Harness::new();
        harness.filter(b"\x1b[6 q");
        let signals = harness
            .scanner
            .take_changed_signals()
            .expect("cursor style changed");
        assert_eq!(signals.restore_bytes(), b"\x1b[6 q".to_vec());

        harness.filter(b"\x1b[0 q");
        let signals = harness
            .scanner
            .take_changed_signals()
            .expect("cursor style reset");
        assert!(signals.restore_bytes().is_empty());
    }

    #[test]
    fn oversized_signal_payloads_are_ignored() {
        let mut harness = Harness::new();
        let mut chunk = b"\x1b]2;".to_vec();
        chunk.extend(std::iter::repeat_n(b'x', 2048));
        chunk.push(0x07);

        harness.filter(&chunk);
        assert!(harness.scanner.take_changed_signals().is_none());
    }

    // -----------------------------------------------------------------------
    // Client-side signal extraction
    // -----------------------------------------------------------------------

    #[test]
    fn passthrough_extraction_keeps_title_and_progress() {
        let data = b"before\x1b]0;relay build\x07mid\x1b]9;4;3;0\x07after";
        assert_eq!(
            extract_passthrough_osc_sequences(data),
            b"\x1b]0;relay build\x07\x1b]9;4;3;0\x07".to_vec()
        );
    }

    #[test]
    fn passthrough_extraction_preserves_string_terminator() {
        let data = b"\x1b]2;relay\x1b\\tail";
        assert_eq!(
            extract_passthrough_osc_sequences(data),
            b"\x1b]2;relay\x1b\\".to_vec()
        );
    }

    #[test]
    fn passthrough_extraction_skips_other_sequences() {
        let data = b"plain\x1b[2Jtext\x1b]7;file://host/tmp\x07\x1b]9;hello\x07";
        assert!(extract_passthrough_osc_sequences(data).is_empty());
    }

    #[test]
    fn passthrough_extraction_ignores_unterminated_sequence() {
        let data = b"\x1b]0;relay build";
        assert!(extract_passthrough_osc_sequences(data).is_empty());
    }

    #[test]
    fn passthrough_extraction_keeps_backslash_payload_intact() {
        let data = b"\x1b]0;C:\\Users\\me\x07";
        assert_eq!(
            extract_passthrough_osc_sequences(data),
            b"\x1b]0;C:\\Users\\me\x07".to_vec()
        );
    }

    #[test]
    fn cursor_style_params_returns_final_change() {
        assert_eq!(
            last_cursor_style_params(b"a\x1b[2 qb\x1b[6 qc"),
            Some(&b"6"[..])
        );
        assert_eq!(last_cursor_style_params(b"\x1b[ q"), Some(&b""[..]));
        assert_eq!(last_cursor_style_params(b"\x1b[2Jplain\x1b[0m"), None);
    }

    // -----------------------------------------------------------------------
    // ConPTY bare forms (Windows only)
    // -----------------------------------------------------------------------

    #[test]
    #[cfg(windows)]
    fn bare_cursor_reports_are_stripped() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("a[35;1Rb[?35;1Rc"), "abc");
        assert_eq!(harness.filter_text("hello[6nworld[5n!"), "helloworld!");
    }

    #[test]
    #[cfg(windows)]
    fn bare_title_regains_its_escape_introducer() {
        let mut harness = Harness::new();
        assert_eq!(
            harness.filter_text("before]0;relay build\x07after"),
            "before\x1b]0;relay build\x07after"
        );
    }

    #[test]
    #[cfg(windows)]
    fn bare_title_is_not_double_escaped() {
        let mut harness = Harness::new();
        let text = "before\x1b]0;relay build\x07after";
        let filtered = harness.filter_text(text);
        assert_eq!(filtered, text);
        assert!(!filtered.contains("\x1b\x1b"));
    }

    #[test]
    #[cfg(windows)]
    fn bare_colour_replies_are_stripped() {
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("a]10;rgb:ffff/ffff/ffff\x07b"), "ab");
    }

    #[test]
    #[cfg(windows)]
    fn bare_backslash_is_not_a_terminator() {
        // Windows shells report titles such as `C:\Users\me`; treating the
        // backslash as a mangled string terminator truncated the title and
        // spilled its tail onto the screen.
        let mut harness = Harness::new();
        assert_eq!(
            harness.filter_text("before]0;C:\\Users\\me\x07after"),
            "before\x1b]0;C:\\Users\\me\x07after"
        );
    }

    #[test]
    #[cfg(windows)]
    fn truncated_bare_candidate_is_not_held_back() {
        // Text ending in `[12` is far more likely to be program output than a
        // split cursor report, and holding it back would leave it stuck on
        // screen while the child is idle.
        let mut harness = Harness::new();
        assert_eq!(harness.filter_text("see [12"), "see [12");
        assert!(harness.pending().is_empty());
    }

    #[test]
    #[cfg(not(windows))]
    fn bracketed_text_is_never_treated_as_a_sequence() {
        let mut harness = Harness::new();
        let text = "matrix[35;1R] and ]0;not-a-title\x07";
        assert_eq!(harness.filter_text(text), text);
        assert!(harness.pending().is_empty());
    }
}
