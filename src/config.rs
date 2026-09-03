use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use crate::error::Result;

/// Default for [`AppConfig::screen_scrollback_rows`].
pub const DEFAULT_SCREEN_SCROLLBACK_ROWS: usize = 5000;

/// Default prompt patterns used to detect interactive prompts in terminal output.
/// These are intentionally broad to cover common shells, REPLs, and CLI tools.
///
/// To override, set `prompt_patterns` in your config file
/// (`~/.local/share/oly/config.json` on Linux/macOS, `%LOCALAPPDATA%\oly\config.json` on Windows):
///
/// ```json
/// {
///     "prompt_patterns": [
///         ">\\s*$",
///         "(?i)password:",
///         "… your own patterns here"
///     ]
/// }
/// ```
const DEFAULT_PROMPT_PATTERNS: &[&str] = &[
    // Shell / REPL prompt characters at end of line
    r"[>❯›\$#%]\s*$",
    r"❯\s+",
    // `> text` at start of line (e.g. Gemini CLI input field)
    r"^\s*>\s+\S",
    // Python REPL
    r">>>\s*$",
    // Confirmation dialogs: (y/n), [y/n], [yes/no]
    r"(?i)[\(\[](y/n|yes/no)[\)\]]",
    // Credential / secret prompts
    r"(?i)(?:password|api[_ ]?key|token|secret)\s*:",
    // Inquirer-style "? " prefix
    r"^\?\s",
    // Natural-language questions ending with "?"
    r"(?i)(?:do you|are you sure|allow\b).{0,80}\?",
    // "Continue?" at end of line
    r"(?i)continue\?\s*$",
    // Press key to continue
    r"(?i)press (?:enter|return|any key)",
];

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub http_bind: String,
    pub http_port: u16,
    pub log_level: String,
    pub stop_grace_seconds: u64,
    pub prompt_patterns: Vec<String>,
    pub web_push_subject: Option<String>,
    pub web_push_vapid_public_key: Option<String>,
    pub web_push_vapid_private_key: Option<String>,
    pub web_push_proxy: Option<String>,
    pub state_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub db_file: PathBuf,
    pub lock_file: PathBuf,
    pub info_file: PathBuf,
    pub socket_name: String,
    pub socket_file: PathBuf,
    pub silence_seconds: u64,
    pub session_eviction_seconds: u64,
    pub max_running_sessions: usize,
    /// Rows of scrolled-off output each session's live screen parser retains
    /// in memory, rendered as scrollback history for freshly attaching
    /// clients.  vt100 only keeps scrollback for the main screen, so
    /// alternate-screen TUIs are unaffected.
    pub screen_scrollback_rows: usize,
    /// Optional path to an executable invoked on every local OS notification.
    /// If this is provided, the default local notification mechanism is disabled and this hook is used instead.
    pub notification_hook: Option<String>,
    /// CLI/runtime flag overrides, recorded so hot reloads can re-apply them:
    /// a value passed on the command line keeps winning over `config.json`
    /// even after the file is edited.
    pub runtime_overrides: RuntimeOverrides,
}

/// CLI/runtime flag overrides that take precedence over `config.json`.
#[derive(Clone, Debug, Default)]
pub struct RuntimeOverrides {
    pub http_bind: Option<String>,
    pub http_port: Option<u16>,
    pub notification_hook: Option<String>,
    pub web_push_proxy: Option<String>,
}

/// Shared, hot-reloadable view of the daemon's configuration.
///
/// The daemon wraps its [`AppConfig`] in this at startup, and a background
/// task swaps in a rebuilt config whenever `config.json` changes on disk.
/// Subsystems that support live updates call [`LiveConfig::get`] at use
/// time; everything else keeps the startup snapshot it was handed.
#[derive(Clone)]
pub struct LiveConfig {
    current: Arc<arc_swap::ArcSwap<AppConfig>>,
}

impl LiveConfig {
    pub fn from_arc(config: Arc<AppConfig>) -> Self {
        Self {
            current: Arc::new(arc_swap::ArcSwap::from(config)),
        }
    }

    /// The current configuration snapshot.
    pub fn get(&self) -> Arc<AppConfig> {
        self.current.load_full()
    }

    /// Swap in a freshly loaded configuration.
    pub fn replace(&self, config: AppConfig) {
        self.current.store(Arc::new(config));
    }
}

#[derive(Debug, Default, Deserialize)]
struct AppConfigOverrides {
    bind: Option<String>,
    http_port: Option<u16>,
    log_level: Option<String>,
    silence_seconds: Option<u64>,
    stop_grace_seconds: Option<u64>,
    prompt_patterns: Option<Vec<String>>,
    web_push_subject: Option<String>,
    web_push_vapid_public_key: Option<String>,
    web_push_vapid_private_key: Option<String>,
    web_push_proxy: Option<String>,
    max_running_sessions: Option<usize>,
    session_eviction_seconds: Option<u64>,
    screen_scrollback_rows: Option<usize>,
    /// Path to an executable invoked on every local OS notification.
    /// Event data is provided via environment variables (OLY_EVENT_*).
    notification_hook: Option<String>,
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        let state_dir = crate::storage::resolve_state_dir();
        ensure_config_file(&state_dir);
        let overrides = load_overrides(&state_dir);
        Ok(Self::resolve(state_dir, overrides))
    }

    /// Build a fully-resolved config from parsed `config.json` overrides.
    fn resolve(state_dir: PathBuf, overrides: AppConfigOverrides) -> Self {
        let sessions_dir = state_dir.join("sessions");
        let session_eviction_seconds = overrides.session_eviction_seconds.unwrap_or(15).max(1);
        let silence_seconds = overrides.silence_seconds.unwrap_or(10).max(1);
        let stop_grace_seconds = overrides.stop_grace_seconds.unwrap_or(5).max(1);
        let http_bind = overrides
            .bind
            .and_then(normalize_optional_string)
            .unwrap_or_else(|| "127.0.0.1".to_string());
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
        let web_push_proxy = resolve_optional_string_setting(
            None,
            std::env::var("OLY_WEB_PUSH_PROXY").ok(),
            overrides.web_push_proxy,
        );
        let socket_name = std::env::var("OLY_SOCKET_NAME")
            .ok()
            .and_then(normalize_optional_string)
            .unwrap_or_else(|| "open-relay.oly.sock".to_string());

        let max_running_sessions = overrides.max_running_sessions.unwrap_or(50);
        let screen_scrollback_rows = overrides
            .screen_scrollback_rows
            .unwrap_or(DEFAULT_SCREEN_SCROLLBACK_ROWS);
        let notification_hook = overrides
            .notification_hook
            .and_then(normalize_optional_string);

        Self {
            log_level,
            silence_seconds,
            stop_grace_seconds,
            session_eviction_seconds,
            http_bind,
            http_port,
            prompt_patterns,
            web_push_vapid_public_key,
            web_push_vapid_private_key,
            web_push_subject,
            web_push_proxy,
            socket_name,
            socket_file: state_dir.join("daemon.sock"),
            lock_file: state_dir.join("daemon.lock"),
            info_file: state_dir.join("daemon.info"),
            db_file: state_dir.join("oly.db"),
            state_dir,
            sessions_dir,
            max_running_sessions,
            screen_scrollback_rows,
            notification_hook,
            runtime_overrides: RuntimeOverrides::default(),
        }
    }

    /// Re-read `config.json` and rebuild the configuration, keeping
    /// process-fixed paths and CLI/runtime overrides.
    ///
    /// Returns an error when the file cannot be read or parsed, so the
    /// daemon's hot-reload loop keeps running on the last good configuration
    /// instead of silently falling back to defaults.
    pub fn try_reload(&self) -> std::result::Result<Self, String> {
        let overrides = try_load_overrides(&self.state_dir)?;
        let mut next = Self::resolve(self.state_dir.clone(), overrides);
        next.runtime_overrides = self.runtime_overrides.clone();
        next.apply_runtime_overrides();
        Ok(next)
    }

    /// Names of hot-reloadable fields that differ between `self` and `other`.
    ///
    /// Everything listed here is picked up by the running daemon without a
    /// restart; keep this in sync with the reload task in
    /// `daemon::reload` and the live readers (notification monitor, session
    /// start paths, HTTP handlers).
    pub fn hot_reload_changes(&self, other: &Self) -> Vec<&'static str> {
        let mut changed = Vec::new();
        if self.log_level != other.log_level {
            changed.push("log_level");
        }
        if self.silence_seconds != other.silence_seconds {
            changed.push("silence_seconds");
        }
        if self.stop_grace_seconds != other.stop_grace_seconds {
            changed.push("stop_grace_seconds");
        }
        if self.prompt_patterns != other.prompt_patterns {
            changed.push("prompt_patterns");
        }
        if self.notification_hook != other.notification_hook {
            changed.push("notification_hook");
        }
        if self.web_push_subject != other.web_push_subject {
            changed.push("web_push_subject");
        }
        if self.web_push_vapid_public_key != other.web_push_vapid_public_key {
            changed.push("web_push_vapid_public_key");
        }
        if self.web_push_vapid_private_key != other.web_push_vapid_private_key {
            changed.push("web_push_vapid_private_key");
        }
        if self.web_push_proxy != other.web_push_proxy {
            changed.push("web_push_proxy");
        }
        if self.max_running_sessions != other.max_running_sessions {
            changed.push("max_running_sessions");
        }
        if self.session_eviction_seconds != other.session_eviction_seconds {
            changed.push("session_eviction_seconds");
        }
        if self.screen_scrollback_rows != other.screen_scrollback_rows {
            changed.push("screen_scrollback_rows");
        }
        changed
    }

    /// Names of fields that differ but only take effect after a daemon
    /// restart (bind address, HTTP port, socket and state paths).
    pub fn restart_required_changes(&self, other: &Self) -> Vec<&'static str> {
        let mut changed = Vec::new();
        if self.http_bind != other.http_bind {
            changed.push("bind");
        }
        if self.http_port != other.http_port {
            changed.push("http_port");
        }
        changed
    }

    pub fn with_runtime_overrides(
        mut self,
        http_bind: Option<String>,
        http_port: Option<u16>,
        notification_hook: Option<String>,
        web_push_proxy: Option<String>,
    ) -> Self {
        self.runtime_overrides = RuntimeOverrides {
            http_bind: http_bind.and_then(normalize_optional_string),
            http_port,
            notification_hook: notification_hook.and_then(normalize_optional_string),
            web_push_proxy: web_push_proxy.and_then(normalize_optional_string),
        };
        self.apply_runtime_overrides();
        self
    }

    /// Re-apply the recorded CLI/runtime overrides on top of file-loaded
    /// values. Runtime flags always win over `config.json`, including after
    /// a hot reload.
    fn apply_runtime_overrides(&mut self) {
        if let Some(http_bind) = &self.runtime_overrides.http_bind {
            self.http_bind = http_bind.clone();
        }
        if let Some(http_port) = self.runtime_overrides.http_port {
            self.http_port = http_port;
        }
        if let Some(notification_hook) = &self.runtime_overrides.notification_hook {
            self.notification_hook = Some(notification_hook.clone());
        }
        if let Some(web_push_proxy) = &self.runtime_overrides.web_push_proxy {
            self.web_push_proxy = Some(web_push_proxy.clone());
        }
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
            Ok(()) => {
                // Restrict permissions to owner-only since the file contains
                // the VAPID private key.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Err(err) =
                        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                    {
                        eprintln!(
                            "warning: could not restrict config.json permissions to 0o600: {err}"
                        );
                    }
                }
                eprintln!("info: generated default config at {}", path.display());
            }
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

/// Strict override loading for hot reload: unlike [`load_overrides`], read
/// and parse failures are reported so the caller can keep the last good
/// configuration instead of silently resetting to defaults.
fn try_load_overrides(state_dir: &Path) -> std::result::Result<AppConfigOverrides, String> {
    let path = state_dir.join("config.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|err| format!("could not read {}: {err}", path.display()))?;
    serde_json::from_str::<AppConfigOverrides>(&raw)
        .map_err(|err| format!("could not parse {}: {err}", path.display()))
}

fn normalize_optional_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn resolve_optional_string_setting(
    runtime_value: Option<String>,
    env_value: Option<String>,
    config_value: Option<String>,
) -> Option<String> {
    runtime_value
        .and_then(normalize_optional_string)
        .or_else(|| env_value.and_then(normalize_optional_string))
        .or_else(|| config_value.and_then(normalize_optional_string))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::AppConfig;

    fn test_config() -> AppConfig {
        let state_dir = PathBuf::from("test-state");
        AppConfig {
            http_bind: "127.0.0.1".to_string(),
            http_port: 15443,
            log_level: "info".to_string(),
            stop_grace_seconds: 5,
            prompt_patterns: Vec::new(),
            web_push_subject: None,
            web_push_vapid_public_key: None,
            web_push_vapid_private_key: None,
            web_push_proxy: Some("http://config-proxy:8080".to_string()),
            state_dir: state_dir.clone(),
            sessions_dir: state_dir.join("sessions"),
            db_file: state_dir.join("oly.db"),
            lock_file: state_dir.join("daemon.lock"),
            info_file: state_dir.join("daemon.info"),
            socket_name: "test.sock".to_string(),
            socket_file: state_dir.join("daemon.sock"),
            silence_seconds: 10,
            session_eviction_seconds: 15,
            max_running_sessions: 50,
            screen_scrollback_rows: super::DEFAULT_SCREEN_SCROLLBACK_ROWS,
            notification_hook: Some("config-hook".to_string()),
            runtime_overrides: Default::default(),
        }
    }

    #[test]
    fn runtime_overrides_replace_port_and_notification_hook() {
        let config = test_config().with_runtime_overrides(
            Some(" 0.0.0.0 ".to_string()),
            Some(17000),
            Some("  C:/tools/notify.exe  ".to_string()),
            Some("  socks5://127.0.0.1:1080  ".to_string()),
        );

        assert_eq!(config.http_bind, "0.0.0.0");
        assert_eq!(config.http_port, 17000);
        assert_eq!(
            config.notification_hook.as_deref(),
            Some("C:/tools/notify.exe")
        );
        assert_eq!(
            config.web_push_proxy.as_deref(),
            Some("socks5://127.0.0.1:1080")
        );
    }

    #[test]
    fn runtime_overrides_leave_config_values_when_not_provided() {
        let config = test_config().with_runtime_overrides(None, None, None, None);

        assert_eq!(config.http_bind, "127.0.0.1");
        assert_eq!(config.http_port, 15443);
        assert_eq!(config.notification_hook.as_deref(), Some("config-hook"));
        assert_eq!(
            config.web_push_proxy.as_deref(),
            Some("http://config-proxy:8080")
        );
    }

    #[test]
    fn optional_string_setting_prefers_runtime_over_env_over_config() {
        let resolved = super::resolve_optional_string_setting(
            Some("  socks5://runtime:1080 ".to_string()),
            Some(" http://env:8080 ".to_string()),
            Some(" http://config:8000 ".to_string()),
        );

        assert_eq!(resolved.as_deref(), Some("socks5://runtime:1080"));
    }

    #[test]
    fn optional_string_setting_ignores_blank_higher_priority_values() {
        let resolved = super::resolve_optional_string_setting(
            Some("   ".to_string()),
            Some("  ".to_string()),
            Some(" http://config:8000 ".to_string()),
        );

        assert_eq!(resolved.as_deref(), Some("http://config:8000"));
    }

    #[test]
    fn screen_scrollback_rows_override_deserializes() {
        let overrides: super::AppConfigOverrides =
            serde_json::from_str(r#"{"screen_scrollback_rows": 250}"#).expect("parse override");
        assert_eq!(overrides.screen_scrollback_rows, Some(250));

        let empty: super::AppConfigOverrides = serde_json::from_str("{}").expect("parse empty");
        assert_eq!(empty.screen_scrollback_rows, None);
    }

    #[test]
    fn try_reload_picks_up_config_file_changes() {
        let state_dir =
            std::env::temp_dir().join(format!("oly_config_reload_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        std::fs::write(
            state_dir.join("config.json"),
            r#"{"notification_hook": "old-hook", "silence_seconds": 42}"#,
        )
        .expect("write config.json");

        let mut config = test_config();
        config.state_dir = state_dir.clone();

        let reloaded = config.try_reload().expect("reload should succeed");
        assert_eq!(reloaded.notification_hook.as_deref(), Some("old-hook"));
        assert_eq!(reloaded.silence_seconds, 42);
        // Fields absent from the file fall back to defaults, not to the
        // previous in-memory values.
        assert_eq!(reloaded.max_running_sessions, 50);

        std::fs::write(
            state_dir.join("config.json"),
            r#"{"notification_hook": "new-hook", "max_running_sessions": 7}"#,
        )
        .expect("rewrite config.json");

        let reloaded = config.try_reload().expect("second reload should succeed");
        assert_eq!(reloaded.notification_hook.as_deref(), Some("new-hook"));
        assert_eq!(reloaded.max_running_sessions, 7);
        assert_eq!(reloaded.silence_seconds, 10);

        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn try_reload_rejects_unparseable_config() {
        let state_dir =
            std::env::temp_dir().join(format!("oly_config_reload_bad_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        std::fs::write(state_dir.join("config.json"), "{ not json").expect("write config.json");

        let mut config = test_config();
        config.state_dir = state_dir.clone();

        assert!(
            config.try_reload().is_err(),
            "a broken config.json must not replace the running configuration"
        );

        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn try_reload_keeps_runtime_overrides_winning_over_file() {
        let state_dir =
            std::env::temp_dir().join(format!("oly_config_reload_rt_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        std::fs::write(
            state_dir.join("config.json"),
            r#"{"notification_hook": "file-hook", "http_port": 19000}"#,
        )
        .expect("write config.json");

        let mut config = test_config();
        config.state_dir = state_dir.clone();
        let config =
            config.with_runtime_overrides(None, Some(17000), Some("cli-hook".to_string()), None);

        let reloaded = config.try_reload().expect("reload should succeed");
        assert_eq!(
            reloaded.notification_hook.as_deref(),
            Some("cli-hook"),
            "CLI flag must keep winning over the edited config file"
        );
        assert_eq!(reloaded.http_port, 17000);

        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn hot_reload_changes_lists_only_hot_fields() {
        let base = test_config();
        let mut changed = base.clone();
        changed.notification_hook = Some("other-hook".to_string());
        changed.http_port = 1;

        assert_eq!(base.hot_reload_changes(&changed), vec!["notification_hook"]);
        assert_eq!(base.restart_required_changes(&changed), vec!["http_port"]);
    }
}
