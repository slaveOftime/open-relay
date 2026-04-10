/// Compiles a list of pattern strings into `Regex` objects.
/// Invalid patterns are skipped with a warning.
pub fn compile_prompt_patterns(patterns: &[String]) -> Vec<regex::Regex> {
    patterns
        .iter()
        .filter_map(|pattern| match regex::Regex::new(pattern) {
            Ok(re) => Some(re),
            Err(err) => {
                tracing::warn!(pattern, %err, "invalid prompt pattern, skipping");
                None
            }
        })
        .collect()
}

/// Returns `true` if any line of `raw_excerpt` matches any compiled pattern.
/// Convenience wrapper around [`find_prompt_match`].
#[cfg_attr(not(test), allow(dead_code))]
pub fn matches_prompt(raw_excerpt: &str, patterns: &[regex::Regex]) -> bool {
    find_prompt_match(raw_excerpt, patterns).is_some()
}

/// Returns the source string of the first pattern that matches any line of
/// `raw_excerpt` (after ANSI stripping), or `None` if nothing matched.
/// Used by the daemon to include the matched rule in log output.
pub fn find_prompt_match(raw_excerpt: &str, patterns: &[regex::Regex]) -> Option<String> {
    if patterns.is_empty() || raw_excerpt.is_empty() {
        return None;
    }
    let clean = strip_ansi(raw_excerpt);
    for line in clean.lines().filter(|line| !line.trim().is_empty()) {
        for re in patterns {
            if re.is_match(line) {
                return Some(re.as_str().to_string());
            }
        }
    }
    None
}

/// Strips common ANSI/VT100 escape sequences from `input`. Pub for use in
/// daemon body extraction.
pub fn strip_ansi_for_body(input: &str) -> String {
    // Also strip bare CPR sequences (`[N;NR` without ESC) that ConPTY on
    // Windows sometimes emits without the leading ESC byte.
    use std::sync::OnceLock;
    static BARE_CPR: OnceLock<regex::Regex> = OnceLock::new();
    let re =
        BARE_CPR.get_or_init(|| regex::Regex::new(r"\x1b\[\??\d+;\d+R|\[\??\d+;\d+R").unwrap());
    let stripped = re.replace_all(input, "");
    strip_ansi(&stripped)
}

/// Normalizes notification body content to language-oriented text.
///
/// Kept:
/// - Unicode alphanumeric characters (`char::is_alphanumeric`), including non-Latin scripts.
/// - Common punctuation: `.,;:!?'-_()[]{}"/@#&+*=<>%$\\|`
/// - Whitespace collapsed to a single ASCII space.
///
/// Dropped:
/// - Emoji, control characters, and other symbols.
pub fn sanitize_body(s: &str) -> String {
    const COMMON_PUNCTUATION: &str = ".,;:!?'-_()[]{}\"/@#&+*=<>%$\\|";

    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;

    for ch in s.chars() {
        if ch.is_alphanumeric() || COMMON_PUNCTUATION.contains(ch) {
            out.push(ch);
            last_was_space = false;
        } else if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            // drop
        }
    }

    out.trim().to_string()
}

/// Strips common ANSI/VT100 escape sequences from `input`.
fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                let _ = chars.next();
                while let Some(c) = chars.next() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                let _ = chars.next();
                let mut prev_esc = false;
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if prev_esc && c == '\\' {
                        break;
                    }
                    prev_esc = c == '\x1b';
                }
            }
            _ => {
                let _ = chars.next();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns(strs: &[&str]) -> Vec<regex::Regex> {
        compile_prompt_patterns(&strs.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn default_patterns() -> Vec<regex::Regex> {
        let cfg = crate::config::AppConfig::load().expect("default config");
        compile_prompt_patterns(&cfg.prompt_patterns)
    }

    // ── strip_ansi ───────────────────────────────────────────────────────

    #[test]
    fn test_strip_ansi_plain_unchanged() {
        assert_eq!(strip_ansi("hello world"), "hello world");
    }

    #[test]
    fn test_strip_ansi_csi_removed() {
        assert_eq!(strip_ansi("\x1b[0mhello\x1b[0m"), "hello");
    }

    #[test]
    fn test_strip_ansi_osc_removed() {
        assert_eq!(strip_ansi("\x1b]0;title\x07prompt> "), "prompt> ");
    }

    // ── compile / find_prompt_match infrastructure ───────────────────────

    #[test]
    fn test_compile_valid_patterns() {
        let compiled = patterns(&[r"(?i)password:", r">\s*$"]);
        assert_eq!(compiled.len(), 2);
    }

    #[test]
    fn test_compile_invalid_pattern_skipped() {
        let compiled = compile_prompt_patterns(&["[invalid".to_string(), r">\s*$".to_string()]);
        assert_eq!(compiled.len(), 1);
    }

    #[test]
    fn test_default_patterns_all_compile() {
        let p = default_patterns();
        assert!(p.len() >= 5, "should have a reasonable number of defaults");
    }

    #[test]
    fn test_empty_excerpt_returns_false() {
        assert!(!matches_prompt("", &default_patterns()));
    }

    #[test]
    fn test_empty_patterns_returns_false() {
        assert!(!matches_prompt("Enter password:", &[]));
    }

    #[test]
    fn test_strips_ansi_before_matching() {
        let p = default_patterns();
        assert!(matches_prompt("\x1b[1mEnter password:\x1b[0m", &p));
    }

    #[test]
    fn test_multiline_any_line_can_match() {
        let p = default_patterns();
        assert!(matches_prompt("some output\nmore output\nmyrepl> ", &p));
    }

    #[test]
    fn test_multiline_no_match() {
        let p = default_patterns();
        assert!(!matches_prompt(
            "some output\nmore output\nno prompt here",
            &p
        ));
    }

    // ── default patterns cover common prompts ────────────────────────────

    #[test]
    fn test_defaults_match_shell_prompts() {
        let p = default_patterns();
        assert!(matches_prompt("myrepl> ", &p));
        assert!(matches_prompt("user@host:~$ ", &p));
        assert!(matches_prompt("$", &p));
        assert!(matches_prompt(">>> ", &p));
    }

    #[test]
    fn test_defaults_match_gemini_input_field() {
        let p = default_patterns();
        assert!(matches_prompt(
            " >   Type your message or @path/to/file",
            &p
        ));
    }

    #[test]
    fn test_defaults_match_confirmations() {
        let p = default_patterns();
        assert!(matches_prompt("Overwrite file? (y/n)", &p));
        assert!(matches_prompt("Delete file? [Y/n]", &p));
        assert!(matches_prompt("Proceed? [Yes/No]", &p));
        assert!(matches_prompt("Do you want to proceed?", &p));
        assert!(matches_prompt("Are you sure you want to delete?", &p));
        assert!(matches_prompt("Continue?", &p));
        assert!(matches_prompt("Allow tool use?", &p));
    }

    #[test]
    fn test_defaults_match_credential_prompts() {
        let p = default_patterns();
        assert!(matches_prompt("Enter password:", &p));
        assert!(matches_prompt("API key:", &p));
        assert!(matches_prompt("Token:", &p));
    }

    #[test]
    fn test_defaults_match_inquirer_and_press_key() {
        let p = default_patterns();
        assert!(matches_prompt("? Which model do you prefer?", &p));
        assert!(matches_prompt("Press ENTER to continue", &p));
        assert!(matches_prompt("Press any key to exit", &p));
    }

    #[test]
    fn test_defaults_match_copilot_cli_input_prompt() {
        let p = default_patterns();
        assert!(matches_prompt(
            "────────────────────────────────────────────────────────────────────────────────────────────────────────
❯  Type @ to mention files, # for issues/PRs, / for commands, or ? for shortcuts
────────────────────────────────────────────────────────────────────────────────────────────────────────
 shift+tab switch mode",
            &p,
        ));
    }

    #[test]
    fn test_defaults_no_false_positives() {
        let p = default_patterns();
        assert!(!matches_prompt("hello world", &p));
        assert!(!matches_prompt("compiling crate v0.1.0", &p));
        assert!(!matches_prompt("your token is ready", &p));
    }

    // ── sanitize_body ────────────────────────────────────────────────────

    #[test]
    fn test_sanitize_body_keeps_letters_spaces_and_punctuation() {
        let input = "  123  Héllo, 世界!   [ok]?  ";
        assert_eq!(sanitize_body(input), "123 Héllo, 世界! [ok]?");
    }

    #[test]
    fn test_sanitize_body_drops_emoji_and_symbols() {
        let input = "Ready ✅ @ 42% -> go🚀";
        assert_eq!(sanitize_body(input), "Ready @ 42% -> go");
    }
}
