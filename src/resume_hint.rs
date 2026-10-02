//! Best-effort resume hints from the journal tail.
//!
//! Two passes, in order:
//!
//! 1. **Engine-rendered pass**: replay the filtered journal tail through
//!    the terminal engine and regex-match the rendered string. This is
//!    what the user actually saw on screen.
//! 2. **Filtered-stream fallback**: when the engine pass produces no
//!    match, regex-match the filtered-stream tail bytes directly with a
//!    much wider line window. The engine walks rows in paint order; if a
//!    TUI paints a resume hint and a later redraw overwrites the row, the
//!    engine drops the hint even though the bytes survive in the journal.
//!    The fallback recovers the hint in that case at the cost of an extra
//!    replay.
//!
//! These are metadata, not commands to execute automatically or a second
//! log store.

use std::path::Path;

use regex::Regex;

use crate::{
    config::ResumePattern,
    error::Result,
    session::{
        journal::JOURNAL_DIR_NAME,
        logs::{RESUME_FALLBACK_TAIL_LINES, render_log_session, replay_filtered_tail},
    },
};

const RESUME_TAIL_ROWS: usize = 80;

/// Only accept resume instructions for the actual child program. This avoids
/// mistaking a quoted instruction in an unrelated shell session for a hint.
fn program_name(command: &str) -> &str {
    let name = command.rsplit(['/', '\\']).next().unwrap_or(command);
    if [".exe", ".cmd", ".bat"]
        .iter()
        .any(|ext| name.to_ascii_lowercase().ends_with(ext))
    {
        &name[..name.len() - 4]
    } else {
        name
    }
}

fn detect(command: &str, tail: &str, patterns: &[ResumePattern]) -> Option<String> {
    let name = program_name(command);
    let mut latest = None;
    for rule in patterns {
        if !name.eq_ignore_ascii_case(program_name(&rule.program)) {
            continue;
        }
        // Config load/reload validates these; handle an invalid rule passed
        // directly by a caller without losing other configured matchers.
        let Ok(regex) = Regex::new(&rule.pattern) else {
            continue;
        };
        for captures in regex.captures_iter(tail) {
            let Some(value_match) = captures.get(1) else {
                continue;
            };
            // Reject partial matches where a shell expression was attached to
            // the identifier/path. A truncated hint would be misleading.
            if tail[value_match.end()..]
                .chars()
                .next()
                .is_some_and(|next| "$`;&|><".contains(next))
            {
                continue;
            }
            let mut candidate = String::new();
            captures.expand(&rule.command, &mut candidate);
            if candidate.len() > 2048 || candidate.chars().any(char::is_control) {
                continue;
            }
            let position = captures.get(0).expect("full match").start();
            if latest
                .as_ref()
                .is_none_or(|(previous, _)| position >= *previous)
            {
                latest = Some((position, candidate));
            }
        }
    }
    latest.map(|(_, command)| command)
}

/// Must be called after both child completion and PTY EOF: the final line
/// often arrives *after* the process has exited. Run from a blocking worker.
pub(crate) fn from_journal(
    dir: &Path,
    command: &str,
    patterns: &[ResumePattern],
) -> Result<Option<String>> {
    if !patterns
        .iter()
        .any(|rule| program_name(command).eq_ignore_ascii_case(program_name(&rule.program)))
        || !dir.join(JOURNAL_DIR_NAME).is_dir()
    {
        return Ok(None);
    }
    // Primary pass: replay the tail through the engine (the same path
    // `oly logs` uses) and regex-match the rendered string.
    let (rendered, _) = render_log_session(dir, RESUME_TAIL_ROWS, false, 2000, None)?;
    if let Some(hint) = detect(command, &String::from_utf8_lossy(&rendered), patterns) {
        return Ok(Some(hint));
    }
    // Fallback: when the engine produced no match, scan the filtered
    // journal tail bytes directly with a much wider window. Engine
    // rendering is sequential, so a TUI that paints a resume hint and
    // then later overwrites that row with a final redraw leaves the hint
    // only in the journal bytes.
    let tail = replay_filtered_tail(dir, RESUME_FALLBACK_TAIL_LINES)?;
    Ok(detect(
        command,
        &String::from_utf8_lossy(&tail.bytes),
        patterns,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Example operator-configured rules for the documented codex/pi
    /// tools; resume detection itself has no built-in rules.
    fn configured_patterns() -> Vec<ResumePattern> {
        vec![
            ResumePattern {
                program: "codex".into(),
                pattern: r"(?i)(?:^|[^a-z0-9_])codex(?:\.exe)?[ \t]+resume[ \t]+([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})(?:$|[^a-z0-9-])".into(),
                command: "codex resume $1".into(),
            },
            ResumePattern {
                program: "pi".into(),
                pattern: r#"(?i)(?:^|[^a-z0-9_])pi(?:\.exe)?[ \t]+--session[ \t]+("[a-z0-9_./:\\~ -]{1,1024}"|'[a-z0-9_./:\\~ -]{1,1024}'|[a-z0-9_./:\\~-]{1,1024})"#.into(),
                command: "pi --session $1".into(),
            },
        ]
    }

    fn detect(command: &str, tail: &str) -> Option<String> {
        super::detect(command, tail, &configured_patterns())
    }

    const ID: &str = "0199e6e2-b60e-715d-851f-b8713b7064df";

    #[test]
    fn extracts_last_matching_hint_for_the_child_only() {
        let tail = format!(
            "To resume: codex resume {ID}\nRun codex resume 0199e6e2-b60e-715d-851f-b8713b7064d0\n"
        );
        assert_eq!(
            detect("C:\\tools\\codex.exe", &tail).as_deref(),
            Some("codex resume 0199e6e2-b60e-715d-851f-b8713b7064d0")
        );
        assert_eq!(detect("bash", &tail), None);
        assert_eq!(detect("codex", "codex resume --last"), None);
        assert_eq!(detect("codex", "codex resume not-a-session"), None);
    }

    #[test]
    fn pi_path_handles_quotes_but_not_shell_expansion() {
        assert_eq!(
            detect(
                "/usr/bin/pi",
                "Resume: pi --session '/tmp/a b/session.jsonl'\n"
            )
            .as_deref(),
            Some("pi --session '/tmp/a b/session.jsonl'")
        );
        assert_eq!(detect("pi", "pi --session $(touch /tmp/evil)"), None);
        assert_eq!(detect("pi", "pi --session `whoami`"), None);
        assert_eq!(detect("pi", "pi --session path.jsonl$(oops)"), None);
        assert_eq!(
            detect(
                "C:\\bin\\PI.CMD",
                "pi --session C:\\Users\\me\\session.jsonl"
            )
            .as_deref(),
            Some("pi --session C:\\Users\\me\\session.jsonl")
        );
    }

    #[test]
    fn reads_journal_tail_not_early_output() {
        let dir = std::env::temp_dir().join(format!("oly-resume-test-{}", uuid::Uuid::new_v4()));
        let early = format!("codex resume {ID}\r\n");
        let late = "codex resume 0199e6e2-b60e-715d-851f-b8713b7064d0\r\n";
        let mut output = early.into_bytes();
        for _ in 0..100 {
            output.extend_from_slice(b"unrelated line\r\n");
        }
        output.extend_from_slice(late.as_bytes());
        crate::session::store::testsupport::seed_journal_output(&dir, &output);
        assert_eq!(
            from_journal(&dir, "codex", &configured_patterns())
                .unwrap()
                .as_deref(),
            Some("codex resume 0199e6e2-b60e-715d-851f-b8713b7064d0")
        );
        assert_eq!(
            from_journal(&dir, "bash", &configured_patterns()).unwrap(),
            None
        );
    }

    #[test]
    fn custom_rules_match_the_last_hint() {
        let custom = ResumePattern {
            program: "agent".into(),
            pattern: r"agent --resume ([a-z0-9-]+)".into(),
            command: "agent --restore $1".into(),
        };
        let tail = "agent --resume first\ncodex resume 0199e6e2-b60e-715d-851f-b8713b7064df\nagent --resume second\n";
        assert_eq!(
            super::detect("agent", tail, std::slice::from_ref(&custom)).as_deref(),
            Some("agent --restore second")
        );
        assert_eq!(
            super::detect("codex", tail, std::slice::from_ref(&custom)),
            None
        );
        assert_eq!(
            super::detect("codex", tail, &configured_patterns()).as_deref(),
            Some("codex resume 0199e6e2-b60e-715d-851f-b8713b7064df")
        );
        assert_eq!(
            super::detect("agent", "agent --resume third", &[custom]).as_deref(),
            Some("agent --restore third")
        );
    }

    /// Older alt-screen frames paint a resume hint and then overwrite it
    /// with a later redraw. The sequential terminal engine renders only
    /// the last frame, so the engine-first pass produces no match. The
    /// filtered-stream fallback scans the raw bytes and recovers it.
    #[test]
    fn fallback_recovers_hint_overwritten_by_later_redraw() {
        let dir =
            std::env::temp_dir().join(format!("oly-resume-fallback-{}", uuid::Uuid::new_v4()));
        let uuid = "0199e6e2-b60e-715d-851f-b8713b7064df";
        let mut output = Vec::new();
        output.extend_from_slice(b"\x1b[?1049h");
        output.extend_from_slice(b"\x1b[H\x1b[2J");
        output.extend_from_slice(format!("\x1b[5;1HTo resume: codex resume {uuid}\r\n").as_bytes());
        // Repro: later redraws cover the hint with unrelated rows.
        for r in 1..40 {
            output.extend_from_slice(format!("\x1b[{};1Hconversation row {}\r\n", r, r).as_bytes());
        }
        // Final clear scene, then the program dies.
        output.extend_from_slice(b"\x1b[H\x1b[2J");
        for r in 1..35 {
            output.extend_from_slice(format!("\x1b[{};1Hscrollback row {}\r\n", r, r).as_bytes());
        }
        crate::session::store::testsupport::seed_journal_output(&dir, &output);

        // Sanity: the engine pass alone must NOT find it (this is the bug
        // the fallback exists to fix).
        let (rendered, _) =
            crate::session::logs::render_log_session(&dir, RESUME_TAIL_ROWS, false, 2000, None)
                .expect("engine render");
        let rendered_text = String::from_utf8_lossy(&rendered);
        assert!(
            !rendered_text.contains(uuid),
            "engine should drop the wiped hint; got: {rendered_text}"
        );

        // The combined `from_journal` call (engine + fallback) recovers it.
        assert_eq!(
            from_journal(&dir, "codex", &configured_patterns())
                .unwrap()
                .as_deref(),
            Some(&*format!("codex resume {uuid}"))
        );
    }

    /// Program ends in an empty final frame (an `ESC[2J ESC[H` clear with
    /// no subsequent rows). Anything the TUI painted before the final
    /// clear is gone from the engine grid, but the filtered stream still
    /// carries the hint. The fallback recovers it.
    #[test]
    fn fallback_recovers_hint_when_final_frame_is_cleared() {
        let dir =
            std::env::temp_dir().join(format!("oly-resume-fallback-{}", uuid::Uuid::new_v4()));
        let path = "/tmp/o b/session.jsonl";
        let mut output = Vec::new();
        output.extend_from_slice(b"\x1b[?1049h");
        output.extend_from_slice(b"\x1b[H\x1b[2J");
        output.extend_from_slice(format!("Resume: pi --session '{path}'\r\n").as_bytes());
        for r in 1..30 {
            output.extend_from_slice(format!("\x1b[{};1Hconversation row {}\r\n", r, r).as_bytes());
        }
        // Final clear with no paint after it (process killed).
        output.extend_from_slice(b"\x1b[H\x1b[2J");
        crate::session::store::testsupport::seed_journal_output(&dir, &output);
        assert_eq!(
            from_journal(&dir, "pi", &configured_patterns())
                .unwrap()
                .as_deref(),
            Some(&*format!("pi --session '{path}'"))
        );
    }

    /// Non-alt session: hint is buried deep in scrollback. Engine shows
    /// only the last 80 lines; the wider filtered-stream window covers it.
    #[test]
    fn fallback_recovers_hint_buried_in_scrollback() {
        let dir =
            std::env::temp_dir().join(format!("oly-resume-fallback-{}", uuid::Uuid::new_v4()));
        let uuid = "0199e6e2-b60e-715d-851f-b8713b7064df";
        let mut output = Vec::new();
        // Hint printed near the START of a long scrollback.
        output.extend_from_slice(format!("To resume: codex resume {uuid}\r\n").as_bytes());
        // Then 250 unrelated lines (above the 80-line engine viewport).
        for i in 0..250 {
            output.extend_from_slice(format!("scrollback line {i:03}\r\n").as_bytes());
        }
        // End on an empty line.
        output.extend_from_slice(b"\r\n");
        crate::session::store::testsupport::seed_journal_output(&dir, &output);
        let (rendered, _) =
            crate::session::logs::render_log_session(&dir, RESUME_TAIL_ROWS, false, 2000, None)
                .expect("engine render");
        let rendered_text = String::from_utf8_lossy(&rendered);
        assert!(
            !rendered_text.contains(uuid),
            "engine should drop scrollback above row 80; got tail: {rendered_text}"
        );
        assert_eq!(
            from_journal(&dir, "codex", &configured_patterns())
                .unwrap()
                .as_deref(),
            Some(&*format!("codex resume {uuid}"))
        );
    }

    /// The fallback scans the journal bytes; if no hint exists at all,
    /// `from_journal` still returns `None`. This is the unhappy-path
    /// sanity check: the fallback must not invent hints.
    #[test]
    fn fallback_returns_none_when_no_hint_anywhere() {
        let dir =
            std::env::temp_dir().join(format!("oly-resume-fallback-{}", uuid::Uuid::new_v4()));
        let mut output = Vec::new();
        output.extend_from_slice(b"\x1b[?1049h");
        for r in 1..30 {
            output.extend_from_slice(format!("\x1b[{};1Hconversation row {}\r\n", r, r).as_bytes());
        }
        output.extend_from_slice(b"\x1b[H\x1b[2J");
        for r in 1..25 {
            output.extend_from_slice(format!("\x1b[{};1Hscrollback row {}\r\n", r, r).as_bytes());
        }
        crate::session::store::testsupport::seed_journal_output(&dir, &output);
        assert_eq!(
            from_journal(&dir, "codex", &configured_patterns()).unwrap(),
            None
        );
    }
}
