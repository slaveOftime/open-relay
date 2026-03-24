use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::Result;

const DEFAULT_PROMPT_PATTERNS: &[&str] = &[
    // ── universal shell / REPL prompts ───────────────────────────────────
    // Shell/REPL > prompt — bash, zsh, Claude Code input, OpenCode, generic REPLs
    r">\s*$",
    // Shell/REPL >>> prompt — python REPLs
    r">>>\s*$",
    // bash/sh $ prompt
    r"\$\s*$",
    // ── confirmation dialogs ─────────────────────────────────────────────
    // (y/n) / (Y/N) inline style
    r"(?i)\(y/n\)",
    // [Y/n] / [y/N] bracket style — used by many CLIs including Copilot, Gemini
    r"(?i)\[y/n\]",
    // [yes/no] bracket style
    r"(?i)\[yes/no\]",
    // ── credential / secret prompts ──────────────────────────────────────
    // "Enter password:" / "Password:"
    r"(?i)password:",
    // API key / token / secret prompts — Gemini CLI, Copilot setup, OpenCode auth
    r"(?i)(?:api[_ ]?key|token|secret)\s*:",
    // ── AI coding tool prompts ───────────────────────────────────────────
    // "? " prefix — GitHub Copilot CLI and all inquirer.js / @clack/prompts CLIs
    r"^\?\s",
    // "Do you want to proceed?" — Claude Code, Gemini CLI, OpenCode confirmations
    r"(?i)do you want",
    // "Allow this action?" / "Allow tool use?" — Claude Code permission prompts
    r"(?i)allow\b.{0,60}\?",
    // "Continue?" at end of line — many AI agents before a destructive step
    r"(?i)continue\?\s*$",
    // "Are you sure?" — generic destructive-action confirmation
    r"(?i)are you sure",
    // Trust confirmation prompts (e.g., VS Code: "Do you trust the files in this folder?")
    r"(?i)do you \b.{0,120}\?",
    // ── wait-for-keystroke ───────────────────────────────────────────────
    // "Press ENTER to continue" / "Press any key"
    r"(?i)press (?:enter|return|any key)",
];

#[derive(Debug)]
pub struct AppConfig {
    pub http_port: u16,
    pub log_level: String,
    pub ring_buffer_bytes: usize,
    pub stop_grace_seconds: u64,
    pub prompt_patterns: Vec<String>,
    pub web_push_subject: Option<String>,
    pub web_push_vapid_public_key: Option<String>,
    pub web_push_vapid_private_key: Option<String>,
    pub state_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub db_file: PathBuf,
    pub lock_file: PathBuf,
    pub socket_name: String,
    pub socket_file: PathBuf,
    pub silence_seconds: u64,
    pub session_eviction_seconds: u64,
    pub max_running_sessions: usize,
    /// Optional path to an executable invoked on every local OS notification.
    /// If this is provided, the default local notification mechanism is disabled and this hook is used instead.
    pub notification_hook: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AppConfigOverrides {
    http_port: Option<u16>,
    log_level: Option<String>,
    prompt_patterns: Option<Vec<String>>,
    web_push_subject: Option<String>,
    web_push_vapid_public_key: Option<String>,
    web_push_vapid_private_key: Option<String>,
    max_running_sessions: Option<usize>,
    session_eviction_seconds: Option<u64>,
    /// Path to an executable invoked on every local OS notification.
    /// Event data is provided via environment variables (OLY_EVENT_*).
    notification_hook: Option<String>,
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let state_dir = crate::storage::resolve_state_dir();
        ensure_config_file(&state_dir);
        let sessions_dir = state_dir.join("sessions");
        let overrides = load_overrides(&state_dir);
        let session_eviction_seconds = overrides.session_eviction_seconds.unwrap_or(15).max(1);
        let http_port = overrides.http_port.unwrap_or(15443);
        let log_level = overrides
            .log_level
            .and_then(normalize_optional_string)
            .unwrap_or_else(|| "info".to_string());
        let prompt_patterns = overrides.prompt_patterns.unwrap_or_else(|| {
            DEFAULT_PROMPT_PATTERNS
                .iter()
                .map(|pattern| (*pattern).to_string())
                .collect()
        });
        let web_push_vapid_public_key = overrides
            .web_push_vapid_public_key
            .and_then(normalize_optional_string);
        let web_push_vapid_private_key = overrides
            .web_push_vapid_private_key
            .and_then(normalize_optional_string);
        let web_push_subject = overrides
            .web_push_subject
            .and_then(normalize_optional_string);
        let socket_name = std::env::var("OLY_SOCKET_NAME")
            .ok()
            .and_then(normalize_optional_string)
            .unwrap_or_else(|| "open-relay.oly.sock".to_string());

        let max_running_sessions = overrides.max_running_sessions.unwrap_or(50);
        let notification_hook = overrides
            .notification_hook
            .and_then(normalize_optional_string);

        Ok(Self {
            log_level,
            ring_buffer_bytes: 1024 * 128, // 128 KB per session
            silence_seconds: 10,
            stop_grace_seconds: 5,
            session_eviction_seconds,
            http_port,
            prompt_patterns,
            web_push_vapid_public_key,
            web_push_vapid_private_key,
            web_push_subject,
            socket_name,
            socket_file: state_dir.join("daemon.sock"),
            lock_file: state_dir.join("daemon.lock"),
            db_file: state_dir.join("oly.db"),
            state_dir,
            sessions_dir,
            max_running_sessions,
            notification_hook,
        })
    }

    pub fn wwwroot_dir(&self) -> PathBuf {
        self.state_dir.join("wwwroot")
    }
}

// ---------------------------------------------------------------------------
// Default config generation
// ---------------------------------------------------------------------------

/// Encode raw bytes as base64url without padding.
fn base64url_no_pad(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4 + 2) / 3);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | (b2 as u32);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[n as usize & 63] as char);
        }
    }
    out
}

/// Generate a random VAPID (P-256) key pair.
/// Returns `(private_key_base64url, public_key_base64url)`.
fn generate_vapid_keypair() -> (String, String) {
    use p256::elliptic_curve::sec1::ToEncodedPoint as _;
    use rand::RngCore as _;

    // Retry until we land on a valid scalar (astronomically unlikely to loop more than once).
    let secret = loop {
        let mut key_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key_bytes);
        let fb = p256::elliptic_curve::FieldBytes::<p256::NistP256>::from(key_bytes);
        if let Ok(sk) = p256::SecretKey::from_bytes(&fb) {
            break sk;
        }
    };
    let private_b64 = base64url_no_pad(secret.to_bytes().as_ref());
    let public_b64 = base64url_no_pad(secret.public_key().to_encoded_point(false).as_bytes());
    (private_b64, public_b64)
}

/// Create `config.json` with freshly generated VAPID keys if it does not exist.
/// Silently skips on any I/O error so the rest of startup can continue.
pub fn ensure_config_file(state_dir: &Path) {
    let path = state_dir.join("config.json");
    if path.exists() {
        return;
    }
    if let Err(err) = std::fs::create_dir_all(state_dir) {
        eprintln!("warning: could not create state dir: {err}");
        return;
    }
    let (private_key, public_key) = generate_vapid_keypair();
    let contents = serde_json::json!({
        "web_push_vapid_public_key": public_key,
        "web_push_vapid_private_key": private_key,
        "web_push_subject": "mailto:admin@oly.com"
    });
    match serde_json::to_string_pretty(&contents) {
        Ok(json) => match std::fs::write(&path, json) {
            Ok(()) => eprintln!("info: generated default config at {}", path.display()),
            Err(err) => eprintln!("warning: could not write config.json: {err}"),
        },
        Err(err) => eprintln!("warning: could not serialise default config: {err}"),
    }
}

fn load_overrides(state_dir: &PathBuf) -> AppConfigOverrides {
    let path = state_dir.join("config.json");
    let Ok(raw) = std::fs::read_to_string(path) else {
        return AppConfigOverrides::default();
    };

    serde_json::from_str::<AppConfigOverrides>(&raw).unwrap_or_default()
}

fn normalize_optional_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
