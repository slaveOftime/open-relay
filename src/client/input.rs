use std::io::Read;

use crate::{
    cli::InputArgs,
    config::AppConfig,
    error::{AppError, Result},
    ipc,
    protocol::{RpcRequest, RpcResponse},
};

pub async fn run_input(
    config: &AppConfig,
    input_args: InputArgs,
    node: Option<String>,
) -> Result<()> {
    use std::io::IsTerminal;

    let id = input_args.id;
    let mut sent_any = false;
    let key_inputs = parse_key_inputs(&input_args.keys)?;

    let text_input = if input_args.text.is_empty() {
        None
    } else {
        Some(input_args.text.join(" "))
    };

    let stdin_is_terminal = std::io::stdin().is_terminal();
    if text_input.is_none() && key_inputs.is_empty() && stdin_is_terminal {
        return Err(AppError::Protocol(
            "no input provided; pass text args, use --key, or pipe stdin into `oly input <id>`"
                .to_string(),
        ));
    }

    if let Some(data) = text_input {
        send_input(config, &id, data, node.as_deref()).await?;
        sent_any = true;
    }

    for key_input in key_inputs {
        send_input(config, &id, key_input, node.as_deref()).await?;
        sent_any = true;
    }

    if !stdin_is_terminal {
        let mut bytes = Vec::new();
        std::io::stdin().read_to_end(&mut bytes)?;
        if !bytes.is_empty() {
            let data = String::from_utf8_lossy(&bytes).to_string();
            send_input(config, &id, data, node.as_deref()).await?;
            sent_any = true;
        }
    }

    if sent_any {
        println!("Input sent to session {id}. Check output with: `oly logs {id}`");
    } else {
        println!(
            "No input sent. Next: run `oly input {id} <text>` or `oly input {id} --key <key>`"
        );
    }

    Ok(())
}

fn parse_key_inputs(specs: &[String]) -> Result<Vec<String>> {
    let mut parsed = Vec::with_capacity(specs.len());
    let mut pending_modifier: Option<String> = None;

    for spec in specs {
        let trimmed = spec.trim();
        let normalized = trimmed.to_ascii_lowercase();

        if normalized.is_empty() {
            return Err(AppError::Protocol(
                "empty --key value is not allowed".to_string(),
            ));
        }

        if let Some(modifier) = modifier_token(&normalized) {
            if let Some(existing) = pending_modifier.take() {
                return Err(AppError::Protocol(format!(
                    "modifier --key `{existing}` must be followed by a key value before `{modifier}`"
                )));
            }
            pending_modifier = Some(modifier.to_string());
            continue;
        }

        let effective = if let Some(modifier) = pending_modifier.take() {
            format!("{modifier}+{trimmed}")
        } else {
            trimmed.to_string()
        };

        parsed.push(parse_key_spec(&effective)?);
    }

    if let Some(modifier) = pending_modifier {
        return Err(AppError::Protocol(format!(
            "modifier --key `{modifier}` must be followed by a key value"
        )));
    }

    Ok(parsed)
}

async fn send_input(config: &AppConfig, id: &str, data: String, node: Option<&str>) -> Result<()> {
    use crate::protocol::RpcRequest as R;
    let inner = RpcRequest::AttachInput {
        id: id.to_string(),
        data,
    };
    let req = match node {
        None => inner,
        Some(n) => R::NodeProxy {
            node: n.to_string(),
            inner: Box::new(inner),
        },
    };
    match ipc::send_request(config, req).await? {
        RpcResponse::Ack => Ok(()),
        RpcResponse::Error { message } => Err(AppError::DaemonUnavailable(message)),
        _ => Err(AppError::Protocol("unexpected response type".to_string())),
    }
}

// ---------------------------------------------------------------------------
// Key spec parsing
// ---------------------------------------------------------------------------

pub fn parse_key_spec(spec: &str) -> Result<String> {
    let trimmed = spec.trim();
    let normalized = trimmed.to_ascii_lowercase();

    if normalized.is_empty() {
        return Err(AppError::Protocol(
            "empty --key value is not allowed".to_string(),
        ));
    }

    if let Some(sequence) = named_key_sequence(&normalized) {
        return Ok(sequence.to_string());
    }

    if let Some(hex) = parse_hex_sequence(trimmed) {
        return Ok(hex);
    }

    if let Some(ch) = single_char(trimmed) {
        return Ok(ch.to_string());
    }

    if let Some(control_char) = parse_ctrl_key(&normalized) {
        return Ok(control_char.to_string());
    }

    if let Some(shifted) = parse_shift_key(trimmed, &normalized) {
        return Ok(shifted);
    }

    if let Some(caps) = parse_caps_key(trimmed, &normalized) {
        return Ok(caps);
    }

    if let Some(alt) = parse_alt_key(trimmed, &normalized) {
        return Ok(alt);
    }

    if matches!(
        normalized.as_str(),
        "shift" | "alt" | "meta" | "ctrl" | "caps" | "capslock"
    ) {
        return Err(AppError::Protocol(
            "modifier-only --key is not supported; use forms like shift+tab, alt+x, ctrl+c, capslock+a"
                .to_string(),
        ));
    }

    Err(AppError::Protocol(format!(
        "unsupported --key `{spec}`; use named keys (enter/esc/tab/up/down/left/right/home/end/pgup/pgdn/del/ins), ctrl+<char>, alt+<char|named-key>, shift+<char|tab>, or capslock+<letter>"
    )))
}

pub fn named_key_sequence(normalized: &str) -> Option<&'static str> {
    match normalized {
        "enter" | "return" | "cr" => Some("\r"),
        "lf" | "linefeed" => Some("\n"),
        "tab" => Some("\t"),
        "backspace" | "bs" => Some("\x08"),
        "esc" | "escape" => Some("\x1b"),
        "up" => Some("\x1b[A"),
        "down" => Some("\x1b[B"),
        "right" => Some("\x1b[C"),
        "left" => Some("\x1b[D"),
        "home" => Some("\x1b[H"),
        "end" => Some("\x1b[F"),
        "delete" | "del" => Some("\x1b[3~"),
        "insert" | "ins" => Some("\x1b[2~"),
        "pageup" | "pgup" => Some("\x1b[5~"),
        "pagedown" | "pgdn" => Some("\x1b[6~"),
        _ => None,
    }
}

fn parse_ctrl_key(normalized: &str) -> Option<char> {
    let key_char = normalized
        .strip_prefix("ctrl+")
        .or_else(|| normalized.strip_prefix("ctrl-"))?;

    let mut chars = key_char.chars();
    let ch = chars.next()?;
    if chars.next().is_some() || !ch.is_ascii() {
        return None;
    }
    Some(((ch.to_ascii_lowercase() as u8) & 0x1f) as char)
}

fn parse_hex_sequence(raw: &str) -> Option<String> {
    let payload = if raw.starts_with("0x") || raw.starts_with("0X") {
        &raw[2..]
    } else if raw.starts_with("\\x") || raw.starts_with("\\X") {
        &raw[2..]
    } else {
        return None;
    };

    if payload.is_empty() || payload.len() % 2 != 0 {
        return None;
    }

    let mut bytes = Vec::with_capacity(payload.len() / 2);
    let mut idx = 0;
    while idx < payload.len() {
        let pair = &payload[idx..idx + 2];
        let byte = u8::from_str_radix(pair, 16).ok()?;
        bytes.push(byte);
        idx += 2;
    }

    Some(String::from_utf8_lossy(&bytes).to_string())
}

fn modifier_token(normalized: &str) -> Option<&'static str> {
    match normalized {
        "ctrl" | "control" => Some("ctrl"),
        "alt" => Some("alt"),
        "meta" => Some("meta"),
        "shift" => Some("shift"),
        "caps" | "capslock" => Some("capslock"),
        _ => None,
    }
}

fn parse_shift_key(raw: &str, normalized: &str) -> Option<String> {
    let payload = modifier_payload(raw, normalized, &["shift+", "shift-"])?;
    let payload_normalized = payload.to_ascii_lowercase();

    if payload_normalized == "tab" {
        return Some("\x1b[Z".to_string());
    }

    let ch = single_char(payload)?;
    Some(shift_char(ch).to_string())
}

fn parse_caps_key(raw: &str, normalized: &str) -> Option<String> {
    let payload = modifier_payload(
        raw,
        normalized,
        &["caps+", "caps-", "capslock+", "capslock-"],
    )?;
    let ch = single_char(payload)?;
    let resolved = if ch.is_ascii_alphabetic() {
        ch.to_ascii_uppercase()
    } else {
        ch
    };
    Some(resolved.to_string())
}

fn parse_alt_key(raw: &str, normalized: &str) -> Option<String> {
    let payload = modifier_payload(raw, normalized, &["alt+", "alt-", "meta+", "meta-"])?;
    let payload_normalized = payload.to_ascii_lowercase();

    if let Some(sequence) = named_key_sequence(&payload_normalized) {
        return Some(format!("\x1b{sequence}"));
    }

    if let Some(control_char) = parse_ctrl_key(&payload_normalized) {
        return Some(format!("\x1b{control_char}"));
    }

    let ch = single_char(payload)?;
    Some(format!("\x1b{ch}"))
}

fn modifier_payload<'a>(raw: &'a str, normalized: &str, prefixes: &[&str]) -> Option<&'a str> {
    for prefix in prefixes {
        if normalized.starts_with(prefix) {
            let payload = raw[prefix.len()..].trim();
            if !payload.is_empty() {
                return Some(payload);
            }
        }
    }
    None
}

fn single_char(value: &str) -> Option<char> {
    let mut chars = value.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(ch)
}

fn shift_char(ch: char) -> char {
    if ch.is_ascii_lowercase() {
        return ch.to_ascii_uppercase();
    }
    match ch {
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        '`' => '~',
        _ => ch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // named_key_sequence
    // -----------------------------------------------------------------------

    #[test]
    fn test_named_key_enter_variants() {
        assert_eq!(named_key_sequence("enter"), Some("\r"));
        assert_eq!(named_key_sequence("return"), Some("\r"));
        assert_eq!(named_key_sequence("cr"), Some("\r"));
    }

    #[test]
    fn test_named_key_arrows() {
        assert_eq!(named_key_sequence("up"), Some("\x1b[A"));
        assert_eq!(named_key_sequence("down"), Some("\x1b[B"));
        assert_eq!(named_key_sequence("right"), Some("\x1b[C"));
        assert_eq!(named_key_sequence("left"), Some("\x1b[D"));
    }

    #[test]
    fn test_named_key_special() {
        assert_eq!(named_key_sequence("tab"), Some("\t"));
        assert_eq!(named_key_sequence("backspace"), Some("\x08"));
        assert_eq!(named_key_sequence("esc"), Some("\x1b"));
        assert_eq!(named_key_sequence("escape"), Some("\x1b"));
        assert_eq!(named_key_sequence("home"), Some("\x1b[H"));
        assert_eq!(named_key_sequence("end"), Some("\x1b[F"));
        assert_eq!(named_key_sequence("delete"), Some("\x1b[3~"));
        assert_eq!(named_key_sequence("del"), Some("\x1b[3~"));
        assert_eq!(named_key_sequence("insert"), Some("\x1b[2~"));
        assert_eq!(named_key_sequence("ins"), Some("\x1b[2~"));
        assert_eq!(named_key_sequence("pageup"), Some("\x1b[5~"));
        assert_eq!(named_key_sequence("pgup"), Some("\x1b[5~"));
        assert_eq!(named_key_sequence("pagedown"), Some("\x1b[6~"));
        assert_eq!(named_key_sequence("pgdn"), Some("\x1b[6~"));
    }

    #[test]
    fn test_named_key_unknown_returns_none() {
        assert_eq!(named_key_sequence("foobar"), None);
        assert_eq!(named_key_sequence(""), None);
    }

    // -----------------------------------------------------------------------
    // parse_key_spec – ctrl
    // -----------------------------------------------------------------------

    #[test]
    fn test_ctrl_plus_c() {
        // Ctrl-C is ASCII 3
        let result = parse_key_spec("ctrl+c").unwrap();
        assert_eq!(result.as_bytes(), &[3]);
    }

    #[test]
    fn test_ctrl_dash_a() {
        let result = parse_key_spec("ctrl-a").unwrap();
        assert_eq!(result.as_bytes(), &[1]);
    }

    #[test]
    fn test_ctrl_uppercase_treated_as_lowercase() {
        let result = parse_key_spec("CTRL+C").unwrap();
        assert_eq!(result.as_bytes(), &[3]);
    }

    #[test]
    fn test_plain_char_is_allowed() {
        assert_eq!(parse_key_spec("c").unwrap(), "c");
        assert_eq!(parse_key_spec("Z").unwrap(), "Z");
    }

    #[test]
    fn test_hex_key_notation() {
        assert_eq!(parse_key_spec("0x1b").unwrap(), "\x1b");
        assert_eq!(parse_key_spec("\\x1b").unwrap(), "\x1b");
    }

    // -----------------------------------------------------------------------
    // parse_key_spec – shift
    // -----------------------------------------------------------------------

    #[test]
    fn test_shift_letter_uppercases() {
        assert_eq!(parse_key_spec("shift+a").unwrap(), "A");
        assert_eq!(parse_key_spec("shift+z").unwrap(), "Z");
    }

    #[test]
    fn test_shift_tab_produces_backtab_sequence() {
        assert_eq!(parse_key_spec("shift+tab").unwrap(), "\x1b[Z");
        assert_eq!(parse_key_spec("shift-tab").unwrap(), "\x1b[Z");
    }

    #[test]
    fn test_shift_digit_produces_symbol() {
        assert_eq!(parse_key_spec("shift+1").unwrap(), "!");
        assert_eq!(parse_key_spec("shift+2").unwrap(), "@");
    }

    // -----------------------------------------------------------------------
    // parse_key_spec – alt / meta
    // -----------------------------------------------------------------------

    #[test]
    fn test_alt_letter_prepends_escape() {
        let result = parse_key_spec("alt+x").unwrap();
        assert_eq!(result, "\x1bx");
    }

    #[test]
    fn test_meta_letter_same_as_alt() {
        assert_eq!(parse_key_spec("meta+x").unwrap(), "\x1bx");
    }

    #[test]
    fn test_alt_named_key_prepends_escape() {
        // alt+up → ESC followed by up-arrow sequence
        let result = parse_key_spec("alt+up").unwrap();
        assert_eq!(result, "\x1b\x1b[A");
    }

    // -----------------------------------------------------------------------
    // parse_key_spec – capslock
    // -----------------------------------------------------------------------

    #[test]
    fn test_caps_letter_uppercases() {
        assert_eq!(parse_key_spec("caps+a").unwrap(), "A");
        assert_eq!(parse_key_spec("capslock+b").unwrap(), "B");
    }

    #[test]
    fn test_caps_non_alpha_unchanged() {
        assert_eq!(parse_key_spec("caps+1").unwrap(), "1");
    }

    // -----------------------------------------------------------------------
    // parse_key_spec – direct named keys
    // -----------------------------------------------------------------------

    #[test]
    fn test_named_key_via_parse_key_spec() {
        assert_eq!(parse_key_spec("enter").unwrap(), "\r");
        assert_eq!(parse_key_spec("ESC").unwrap(), "\x1b");
        assert_eq!(parse_key_spec("tab").unwrap(), "\t");
    }

    // -----------------------------------------------------------------------
    // parse_key_spec – error paths
    // -----------------------------------------------------------------------

    #[test]
    fn test_empty_key_spec_is_error() {
        assert!(parse_key_spec("").is_err());
        assert!(parse_key_spec("   ").is_err());
    }

    #[test]
    fn test_modifier_only_is_error() {
        assert!(parse_key_spec("ctrl").is_err());
        assert!(parse_key_spec("shift").is_err());
        assert!(parse_key_spec("alt").is_err());
        assert!(parse_key_spec("meta").is_err());
        assert!(parse_key_spec("caps").is_err());
        assert!(parse_key_spec("capslock").is_err());
    }

    #[test]
    fn test_repeatable_modifier_sequence_ctrl_then_char() {
        let specs = vec!["ctrl".to_string(), "c".to_string()];
        let parsed = parse_key_inputs(&specs).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].as_bytes(), &[3]);
    }

    #[test]
    fn test_repeatable_modifier_sequence_alt_then_named_key() {
        let specs = vec!["alt".to_string(), "up".to_string()];
        let parsed = parse_key_inputs(&specs).unwrap();
        assert_eq!(parsed, vec!["\x1b\x1b[A".to_string()]);
    }

    #[test]
    fn test_repeatable_modifier_sequence_missing_value_is_error() {
        let specs = vec!["ctrl".to_string()];
        assert!(parse_key_inputs(&specs).is_err());
    }

    #[test]
    fn test_unsupported_key_is_error() {
        assert!(parse_key_spec("f1").is_err());
        assert!(parse_key_spec("ctrl+ab").is_err()); // multi-char ctrl not supported
    }

    // -----------------------------------------------------------------------
    // parse_key_spec – alt + ctrl combo
    // -----------------------------------------------------------------------

    #[test]
    fn test_alt_ctrl_combo_produces_esc_then_control_char() {
        // alt+ctrl+c → ESC + \x03
        let result = parse_key_spec("alt+ctrl+c").unwrap();
        assert_eq!(result.as_bytes(), &[0x1b, 0x03]);
    }

    // -----------------------------------------------------------------------
    // named_key_sequence – all variants exhaustively
    // -----------------------------------------------------------------------

    #[test]
    fn test_named_key_lf_linefeed() {
        assert_eq!(named_key_sequence("lf"), Some("\n"));
        assert_eq!(named_key_sequence("linefeed"), Some("\n"));
    }

    #[test]
    fn test_named_key_bs_alias() {
        assert_eq!(named_key_sequence("bs"), Some("\x08"));
    }

    // -----------------------------------------------------------------------
    // shift_char – number row and symbols
    // -----------------------------------------------------------------------

    #[test]
    fn test_shift_number_row_complete() {
        assert_eq!(parse_key_spec("shift+3").unwrap(), "#");
        assert_eq!(parse_key_spec("shift+4").unwrap(), "$");
        assert_eq!(parse_key_spec("shift+5").unwrap(), "%");
        assert_eq!(parse_key_spec("shift+6").unwrap(), "^");
        assert_eq!(parse_key_spec("shift+7").unwrap(), "&");
        assert_eq!(parse_key_spec("shift+8").unwrap(), "*");
        assert_eq!(parse_key_spec("shift+9").unwrap(), "(");
        assert_eq!(parse_key_spec("shift+0").unwrap(), ")");
    }

    #[test]
    fn test_shift_punctuation() {
        assert_eq!(parse_key_spec("shift+-").unwrap(), "_");
        assert_eq!(parse_key_spec("shift+=").unwrap(), "+");
        assert_eq!(parse_key_spec("shift+;").unwrap(), ":");
        assert_eq!(parse_key_spec("shift+,").unwrap(), "<");
        assert_eq!(parse_key_spec("shift+.").unwrap(), ">");
        assert_eq!(parse_key_spec("shift+/").unwrap(), "?");
        assert_eq!(parse_key_spec("shift+`").unwrap(), "~");
    }

    // -----------------------------------------------------------------------
    // hex sequence edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_hex_sequence_multi_byte() {
        // Two-byte escape + bracket → \x1b[
        assert_eq!(parse_key_spec("0x1b5b").unwrap(), "\x1b[");
    }

    #[test]
    fn test_hex_sequence_backslash_prefix() {
        assert_eq!(parse_key_spec("\\x03").unwrap(), "\x03");
        assert_eq!(parse_key_spec("\\x1b").unwrap(), "\x1b");
    }

    #[test]
    fn test_hex_sequence_odd_length_is_error() {
        // "0x1" has one hex digit (odd) → error
        assert!(parse_key_spec("0x1").is_err());
    }

    // -----------------------------------------------------------------------
    // parse_key_inputs – multi-key sequence ordering
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_key_inputs_multiple_keys_preserves_order() {
        let specs = vec!["up".to_string(), "enter".to_string(), "tab".to_string()];
        let parsed = parse_key_inputs(&specs).unwrap();
        assert_eq!(
            parsed,
            vec!["\x1b[A".to_string(), "\r".to_string(), "\t".to_string()]
        );
    }

    #[test]
    fn test_parse_key_inputs_empty_slice_returns_empty() {
        let parsed = parse_key_inputs(&[]).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn test_parse_key_inputs_empty_string_is_error() {
        let specs = vec!["".to_string()];
        assert!(parse_key_inputs(&specs).is_err());
    }

    #[test]
    fn test_parse_key_inputs_consecutive_modifiers_is_error() {
        // Two modifiers in a row without a key between them
        let specs = vec!["ctrl".to_string(), "alt".to_string(), "c".to_string()];
        assert!(parse_key_inputs(&specs).is_err());
    }

    // -----------------------------------------------------------------------
    // alt + named navigation keys
    // -----------------------------------------------------------------------

    #[test]
    fn test_alt_arrow_keys() {
        assert_eq!(parse_key_spec("alt+up").unwrap(), "\x1b\x1b[A");
        assert_eq!(parse_key_spec("alt+down").unwrap(), "\x1b\x1b[B");
        assert_eq!(parse_key_spec("alt+right").unwrap(), "\x1b\x1b[C");
        assert_eq!(parse_key_spec("alt+left").unwrap(), "\x1b\x1b[D");
    }

    #[test]
    fn test_alt_home_end() {
        assert_eq!(parse_key_spec("alt+home").unwrap(), "\x1b\x1b[H");
        assert_eq!(parse_key_spec("alt+end").unwrap(), "\x1b\x1b[F");
    }

    // -----------------------------------------------------------------------
    // capslock + non-alpha is passthrough
    // -----------------------------------------------------------------------

    #[test]
    fn test_caps_digit_passthrough() {
        assert_eq!(parse_key_spec("caps+5").unwrap(), "5");
        assert_eq!(parse_key_spec("capslock+0").unwrap(), "0");
    }

    // -----------------------------------------------------------------------
    // ctrl key – all alphabetic chars map to ASCII 1–26
    // -----------------------------------------------------------------------

    #[test]
    fn test_ctrl_full_alphabet() {
        for (letter, expected_byte) in ('a'..='z').zip(1u8..=26u8) {
            let spec = format!("ctrl+{letter}");
            let result = parse_key_spec(&spec).unwrap_or_else(|e| panic!("failed for {spec}: {e}"));
            assert_eq!(
                result.as_bytes(),
                &[expected_byte],
                "failed for ctrl+{letter}"
            );
        }
    }
}
