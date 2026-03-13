use std::io::Read;

use crate::{
    cli::SendArgs,
    config::AppConfig,
    error::{AppError, Result},
    ipc,
    protocol::{RpcRequest, RpcResponse},
};

pub async fn run_send(config: &AppConfig, send_args: SendArgs, node: Option<String>) -> Result<()> {
    use std::io::IsTerminal;

    let id = send_args
        .id
        .expect("session ID must be resolved before run_send");
    let has_chunks = !send_args.chunks.is_empty();
    let stdin_is_terminal = std::io::stdin().is_terminal();

    if !has_chunks && stdin_is_terminal {
        return Err(AppError::Protocol(
            "no input provided; pass text/key chunks or pipe stdin. Example: oly send <id> \"hello\" key:enter"
                .to_string(),
        ));
    }

    let mut sent_any = false;

    // Process ordered chunks left to right
    for chunk in &send_args.chunks {
        let data = resolve_chunk(chunk)?;
        send_data(config, &id, data, node.as_deref()).await?;
        sent_any = true;
    }

    // Piped stdin (only when no explicit chunks were given)
    if !has_chunks && !stdin_is_terminal {
        let mut bytes = Vec::new();
        std::io::stdin().read_to_end(&mut bytes)?;
        if !bytes.is_empty() {
            let data = String::from_utf8_lossy(&bytes).to_string();
            send_data(config, &id, data, node.as_deref()).await?;
            sent_any = true;
        }
    }

    if sent_any {
        println!("Input sent to session {id}. Check output with: `oly logs {id}`");
    }

    Ok(())
}

/// Resolve a single CLI chunk into the bytes to send.
/// - `key:<spec>` → special key sequence
/// - anything else → literal text
fn resolve_chunk(chunk: &str) -> Result<String> {
    if let Some(spec) = chunk.strip_prefix("key:") {
        parse_key_spec(spec)
    } else {
        Ok(chunk.to_string())
    }
}

async fn send_data(config: &AppConfig, id: &str, data: String, node: Option<&str>) -> Result<()> {
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
            "empty key spec is not allowed".to_string(),
        ));
    }

    if let Some(sequence) = named_key_sequence(&normalized) {
        return Ok(sequence.to_string());
    }

    if let Some(hex) = parse_hex_bytes(&normalized) {
        return Ok(hex);
    }

    if let Some(control_char) = parse_ctrl_key(&normalized) {
        return Ok(control_char.to_string());
    }

    if let Some(alt) = parse_alt_key(&normalized) {
        return Ok(alt);
    }

    if normalized == "shift+tab" || normalized == "shift-tab" {
        return Ok("\x1b[Z".to_string());
    }

    if matches!(normalized.as_str(), "shift" | "alt" | "meta" | "ctrl") {
        return Err(AppError::Protocol(
            "modifier-only key is not allowed; use forms like key:ctrl+c, key:alt+x, key:shift+tab"
                .to_string(),
        ));
    }

    Err(AppError::Protocol(format!(
        "unsupported key `{spec}`; use named keys (enter, tab, esc, backspace, up/down/left/right, home/end, pgup/pgdn, del/ins), ctrl+<char>, alt+<char|key>, shift+tab, or hex:<bytes>"
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

/// Parse `hex:<hex-bytes>` notation, e.g. `hex:1b` or `hex:1b5b41`.
fn parse_hex_bytes(normalized: &str) -> Option<String> {
    let payload = normalized.strip_prefix("hex:")?;

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

fn parse_alt_key(normalized: &str) -> Option<String> {
    let payload = normalized
        .strip_prefix("alt+")
        .or_else(|| normalized.strip_prefix("alt-"))
        .or_else(|| normalized.strip_prefix("meta+"))
        .or_else(|| normalized.strip_prefix("meta-"))?;

    if let Some(sequence) = named_key_sequence(payload) {
        return Some(format!("\x1b{sequence}"));
    }

    if let Some(control_char) = parse_ctrl_key(payload) {
        return Some(format!("\x1b{control_char}"));
    }

    let mut chars = payload.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(format!("\x1b{ch}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // resolve_chunk – text vs key:
    // -----------------------------------------------------------------------

    #[test]
    fn text_chunk_passes_through() {
        assert_eq!(resolve_chunk("hello").unwrap(), "hello");
        assert_eq!(resolve_chunk("git status").unwrap(), "git status");
    }

    #[test]
    fn key_chunk_parsed() {
        assert_eq!(resolve_chunk("key:enter").unwrap(), "\r");
        assert_eq!(resolve_chunk("key:ctrl+c").unwrap().as_bytes(), &[3]);
    }

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
    // parse_key_spec – ctrl
    // -----------------------------------------------------------------------

    #[test]
    fn test_ctrl_plus_c() {
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

    // -----------------------------------------------------------------------
    // parse_key_spec – hex
    // -----------------------------------------------------------------------

    #[test]
    fn test_hex_key_notation() {
        assert_eq!(parse_key_spec("hex:1b").unwrap(), "\x1b");
        assert_eq!(parse_key_spec("hex:03").unwrap(), "\x03");
    }

    #[test]
    fn test_hex_sequence_multi_byte() {
        assert_eq!(parse_key_spec("hex:1b5b").unwrap(), "\x1b[");
    }

    #[test]
    fn test_hex_sequence_odd_length_is_error() {
        assert!(parse_key_spec("hex:1").is_err());
    }

    // -----------------------------------------------------------------------
    // parse_key_spec – shift+tab only
    // -----------------------------------------------------------------------

    #[test]
    fn test_shift_tab_produces_backtab_sequence() {
        assert_eq!(parse_key_spec("shift+tab").unwrap(), "\x1b[Z");
        assert_eq!(parse_key_spec("shift-tab").unwrap(), "\x1b[Z");
    }

    // -----------------------------------------------------------------------
    // parse_key_spec – alt / meta
    // -----------------------------------------------------------------------

    #[test]
    fn test_alt_letter_prepends_escape() {
        assert_eq!(parse_key_spec("alt+x").unwrap(), "\x1bx");
    }

    #[test]
    fn test_meta_letter_same_as_alt() {
        assert_eq!(parse_key_spec("meta+x").unwrap(), "\x1bx");
    }

    #[test]
    fn test_alt_named_key_prepends_escape() {
        assert_eq!(parse_key_spec("alt+up").unwrap(), "\x1b\x1b[A");
    }

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

    #[test]
    fn test_alt_ctrl_combo() {
        let result = parse_key_spec("alt+ctrl+c").unwrap();
        assert_eq!(result.as_bytes(), &[0x1b, 0x03]);
    }

    // -----------------------------------------------------------------------
    // parse_key_spec – named keys via parse_key_spec
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
    }

    #[test]
    fn test_unsupported_key_is_error() {
        assert!(parse_key_spec("f1").is_err());
        assert!(parse_key_spec("ctrl+ab").is_err());
    }
}
