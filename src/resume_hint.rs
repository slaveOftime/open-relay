//! Best-effort resume hints from the *rendered* journal tail. These are
//! metadata, not commands to execute automatically or a second log store.

use std::path::Path;

use regex::Regex;

use crate::{
    config::ResumePattern,
    error::Result,
    session::{journal::JOURNAL_DIR_NAME, logs::render_log_session},
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
    let (rendered, _) = render_log_session(dir, RESUME_TAIL_ROWS, false, 2000, None)?;
    Ok(detect(
        command,
        &String::from_utf8_lossy(&rendered),
        patterns,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_resume_patterns;

    fn detect(command: &str, tail: &str) -> Option<String> {
        super::detect(command, tail, &default_resume_patterns())
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
            from_journal(&dir, "codex", &default_resume_patterns())
                .unwrap()
                .as_deref(),
            Some("codex resume 0199e6e2-b60e-715d-851f-b8713b7064d0")
        );
        assert_eq!(
            from_journal(&dir, "bash", &default_resume_patterns()).unwrap(),
            None
        );
    }

    #[test]
    fn custom_rules_match_the_last_hint_and_can_replace_defaults() {
        let custom = ResumePattern {
            program: "agent".into(),
            pattern: r"agent --resume ([a-z0-9-]+)".into(),
            command: "agent --restore $1".into(),
        };
        let tail = "agent --resume first\ncodex resume 0199e6e2-b60e-715d-851f-b8713b7064df\nagent --resume second\n";
        assert_eq!(
            super::detect("agent", tail, &[custom.clone()]).as_deref(),
            Some("agent --restore second")
        );
        assert_eq!(super::detect("codex", tail, &[custom.clone()]), None);
        assert_eq!(
            super::detect("codex", tail, &default_resume_patterns()).as_deref(),
            Some("codex resume 0199e6e2-b60e-715d-851f-b8713b7064df")
        );
        assert_eq!(
            super::detect("agent", "agent --resume third", &[custom]).as_deref(),
            Some("agent --restore third")
        );
    }
}
