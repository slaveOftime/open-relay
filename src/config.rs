use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use crate::error::Result;

/// Default for [`AppConfig::screen_scrollback_rows`].
pub const DEFAULT_SCREEN_SCROLLBACK_ROWS: usize = 5000;

/// Default free-space floor (1 GiB) below which `disk_guard_enabled` refuses
/// new sessions. This is an operator-tunable safety margin, not a bound
/// derived from session count: journal growth depends on workload and time.
/// It is a refusal threshold, not a quota or a capacity guarantee.
pub const DEFAULT_DISK_MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024;

/// Default free-space warning threshold (2 GiB): warn (log + status) before
/// the guard's refusal floor is reached. Alerts are always on; the guard
/// itself is opt-in.
pub const DEFAULT_DISK_WARN_FREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Minimum number of scrolled-off rows a fresh attach seeds into the client
/// terminal's scrollback, regardless of the client's own screen height.
/// Without this floor a reattach only restores one screenful of history,
/// which reads as "the session's history got cut". Capped by the session's
/// [`AppConfig::screen_scrollback_rows`] retention.
pub const DEFAULT_ATTACH_SCROLLBACK_SEED_ROWS: usize = 1000;

/// Default prompt patterns used to detect interactive prompts in terminal output.
/// These are intentionally broad to cover common shells, REPLs, and CLI tools.
///
/// To override, set `prompt_patterns` in your config file
/// (`~/.local/state/oly/config.json` on Linux,
/// `~/Library/Application Support/oly/config.json` on macOS,
/// `%LOCALAPPDATA%\oly\config.json` on Windows):
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
pub(crate) const DEFAULT_PROMPT_PATTERNS: &[&str] = &[
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

/// A resume hint advertised in the rendered tail of a completed session.
/// `program` matches the child executable's basename (case-insensitive),
/// ignoring .exe/.cmd/.bat. `pattern` is a Rust regex with at least one
/// capture group; `command` expands `$1`, `$2`, etc. from the match.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ResumePattern {
    pub program: String,
    pub pattern: String,
    pub command: String,
}

/// Configured rules for deriving a resume hint. The detected text is a
/// suggestion only; the daemon must never execute it automatically. The
/// rules come exclusively from the `resume_patterns` key in `config.json`;
/// there are no built-in defaults, so an absent or empty list disables
/// resume detection entirely.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResumeConfig {
    pub patterns: Vec<ResumePattern>,
}

// ── Hot-reload diff helper ─────────────────────────────────────────────────

//
// `AppConfig::hot_reload_changes` and `restart_required_changes` used to be
// two open-coded `if x != y { push("x") }` loops that grew with every new
// setting — easy to add a field and forget to teach one of the lists, which
// meant a config change could silently stop being applied.
//
// The sub-structs below each `impl ConfigDiff`, so the top-level diff
// becomes a composition: add a field to a sub-struct, get its diff arm
// populated by the macro. The two lists still need to make a deliberate
// choice about which sub-struct's diff belongs where, but no per-field
// hand-maintained bookkeeping survives.
trait ConfigDiff {
    fn diff(&self, other: &Self) -> Vec<&'static str>;
}

macro_rules! impl_config_diff {
    ($T:ty { $($field:ident),+ $(,)? }) => {
        impl ConfigDiff for $T {
            fn diff(&self, other: &Self) -> Vec<&'static str> {
                let mut out = Vec::new();
                $(if self.$field != other.$field { out.push(stringify!($field)); })+
                out
            }
        }
    };
}

// ── Sub-structs ─────────────────────────────────────────────────────────────

/// On-disk paths the daemon binds to once at startup.
/// Changing any of these requires a restart (see `restart_required_changes`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathsConfig {
    pub state_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub db_file: PathBuf,
    pub lock_file: PathBuf,
    pub info_file: PathBuf,
    pub socket_name: String,
    pub socket_file: PathBuf,
}

/// HTTP listener bind/port. Hot-reloadable on bind/port by `HttpConfig`'s
/// placement in either diff list; see `restart_required_changes` for which
/// changes need the daemon to actually rebind the listener.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpConfig {
    pub bind: String,
    pub port: u16,
}

/// Notification dispatch tuning (prompt detection cadence, hook, OS hooks).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotifyConfig {
    pub min_interval_seconds: u64,
    pub prompt_patterns: Vec<String>,
    pub hook: Option<String>,
}

/// Resource quotas / runtime tuning knobs (TUI scrollback retention, eviction
/// grace periods, etc.). All reload-safe because they're sampled at use time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LimitsConfig {
    pub max_running_sessions: usize,
    pub session_eviction_seconds: u64,
    pub screen_scrollback_rows: usize,
    pub silence_seconds: u64,
    pub stop_grace_seconds: u64,
    /// Wall-clock retention for **stopped** sessions, in days. The
    /// sweeper deletes the journal directory and DB row for stopped
    /// sessions older than this cap. 0 = disabled; running sessions
    /// are unaffected by this knob.
    pub journal_retention_days: u32,
    /// Disk guard for new sessions: refuse to start when the state
    /// volume's free space is below `disk_min_free_bytes`. Explicit
    /// opt-in (default off); existing sessions are never killed by it.
    pub disk_guard_enabled: bool,
    /// Free-space floor for admitting new sessions, in bytes.
    pub disk_min_free_bytes: u64,
    /// Free-space warning threshold (logged and surfaced in status), in
    /// bytes. Must be >= `disk_min_free_bytes`.
    pub disk_warn_free_bytes: u64,
    /// Background sampling cadence for the state volume, in seconds
    /// (clamped to >= 5).
    pub disk_sample_interval_seconds: u64,
    /// Maximum age of a disk sample the guard still trusts, in seconds.
    /// Unknown or older samples refuse new sessions while the guard is on.
    pub disk_sample_max_age_seconds: u64,
}

/// VAPID keys + optional proxy for browser push subscriptions. Hot-reloadable
/// because the daemon reads them at every push send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebPushConfig {
    pub subject: Option<String>,
    pub vapid_public_key: Option<String>,
    pub vapid_private_key: Option<String>,
    pub proxy: Option<String>,
}

impl_config_diff!(PathsConfig {
    state_dir,
    sessions_dir,
    db_file,
    lock_file,
    info_file,
    socket_name,
    socket_file,
});
impl_config_diff!(HttpConfig { bind, port });
impl_config_diff!(NotifyConfig {
    min_interval_seconds,
    prompt_patterns,
    hook
});
impl_config_diff!(LimitsConfig {
    max_running_sessions,
    session_eviction_seconds,
    screen_scrollback_rows,
    silence_seconds,
    stop_grace_seconds,
    journal_retention_days,
    disk_guard_enabled,
    disk_min_free_bytes,
    disk_warn_free_bytes,
    disk_sample_interval_seconds,
    disk_sample_max_age_seconds,
});
impl_config_diff!(WebPushConfig {
    subject,
    vapid_public_key,
    vapid_private_key,
    proxy,
});
impl_config_diff!(ResumeConfig { patterns });

/// CLI/runtime flag overrides that take precedence over `config.json`.
///
/// Kept flat on purpose: the four overrides each belong to a different
/// sub-struct (http vs notify vs web_push), and at runtime they're
/// applied individually. Wrapping them in their own sub-struct hierarchy
/// would be churn without benefit at this size.
#[derive(Clone, Debug, Default)]
pub struct RuntimeOverrides {
    pub http_bind: Option<String>,
    pub http_port: Option<u16>,
    pub notification_hook: Option<String>,
    pub web_push_proxy: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub paths: PathsConfig,
    pub http: HttpConfig,
    pub notify: NotifyConfig,
    pub limits: LimitsConfig,
    pub web_push: WebPushConfig,
    pub resume: ResumeConfig,
    pub log_level: String,
    /// Opt-in trusted operator content sharing the control-plane origin.
    pub trusted_apps_enabled: bool,
    /// CLI/runtime flag overrides, recorded so hot reloads can re-apply them:
    /// a value passed on the command line keeps winning over `config.json`
    /// even after the file is edited.
    pub runtime_overrides: RuntimeOverrides,
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
    /// On-disk JSON shape stays flat and backward-compatible: each
    /// sub-struct is `#[serde(flatten)]`-ed so the wire format ("bind",
    /// "http_port", "web_push_*", …) is byte-identical to the
    /// pre-flatten layout: existing `config.json` files deserialize
    /// without `unknown field` errors, and new keys land in a
    /// sub-struct automatically.
    #[serde(flatten)]
    http: HttpOverrides,
    #[serde(flatten)]
    notify: NotifyOverrides,
    #[serde(flatten)]
    web_push: WebPushOverrides,
    #[serde(flatten)]
    limits: LimitsOverrides,
    #[serde(flatten)]
    resume: ResumeOverrides,
    log_level: Option<String>,
    trusted_apps_enabled: Option<bool>,
    /// Unconsumed flat keys are reported rather than silently ignored.
    #[serde(flatten)]
    unknown: std::collections::BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
struct HttpOverrides {
    /// Listener bind address. Canonical key is `http_bind` (matching
    /// `http_port`); the legacy single-word `bind` is still accepted as
    /// an alias so existing `config.json` files deserialize cleanly.
    #[serde(alias = "bind")]
    http_bind: Option<String>,
    http_port: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
struct NotifyOverrides {
    notification_min_interval_seconds: Option<u64>,
    notification_hook: Option<String>,
    prompt_patterns: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct ResumeOverrides {
    /// The complete rule list. Absent or empty disables detection.
    resume_patterns: Option<Vec<ResumePattern>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
struct WebPushOverrides {
    web_push_subject: Option<String>,
    web_push_vapid_public_key: Option<String>,
    web_push_vapid_private_key: Option<String>,
    web_push_proxy: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
struct LimitsOverrides {
    silence_seconds: Option<u64>,
    stop_grace_seconds: Option<u64>,
    max_running_sessions: Option<usize>,
    session_eviction_seconds: Option<u64>,
    screen_scrollback_rows: Option<usize>,
    /// Wall-clock retention for stopped sessions. 0 = disabled (keep
    /// everything, the pre-cap behaviour).
    journal_retention_days: Option<u32>,
    /// Disk guard for new sessions; explicit opt-in.
    disk_guard_enabled: Option<bool>,
    /// Free-space floor for new sessions, in bytes.
    disk_min_free_bytes: Option<u64>,
    /// Free-space warning threshold, in bytes.
    disk_warn_free_bytes: Option<u64>,
    /// State-volume sampling cadence, in seconds.
    disk_sample_interval_seconds: Option<u64>,
    /// Maximum age of a sample the guard still trusts, in seconds.
    disk_sample_max_age_seconds: Option<u64>,
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        Self::load_from_state_dir(crate::storage::resolve_state_dir())
    }

    fn load_from_state_dir(state_dir: PathBuf) -> Result<Self> {
        ensure_config_file(&state_dir)?;
        let overrides = try_load_overrides(&state_dir).map_err(crate::error::AppError::Protocol)?;
        let config = Self::resolve(state_dir, overrides);
        config
            .validate()
            .map_err(crate::error::AppError::Protocol)?;
        Ok(config)
    }

    /// Build a fully-resolved config from parsed `config.json` overrides.
    fn resolve(state_dir: PathBuf, overrides: AppConfigOverrides) -> Self {
        let resume_patterns = overrides.resume.resume_patterns.unwrap_or_default();
        let paths = PathsConfig {
            state_dir: state_dir.clone(),
            sessions_dir: state_dir.join("sessions"),
            db_file: state_dir.join("oly.db"),
            lock_file: state_dir.join("daemon.lock"),
            info_file: state_dir.join("daemon.info"),
            socket_name: std::env::var("OLY_SOCKET_NAME")
                .ok()
                .and_then(normalize_optional_string)
                .unwrap_or_else(|| default_socket_name(&state_dir)),
            socket_file: state_dir.join("daemon.sock"),
        };
        let http = HttpConfig {
            bind: overrides
                .http
                .http_bind
                .and_then(normalize_optional_string)
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            port: overrides.http.http_port.unwrap_or(15443),
        };
        let notify = NotifyConfig {
            min_interval_seconds: overrides
                .notify
                .notification_min_interval_seconds
                .unwrap_or(10)
                .max(1),
            prompt_patterns: overrides.notify.prompt_patterns.unwrap_or_else(|| {
                DEFAULT_PROMPT_PATTERNS
                    .iter()
                    .map(|p| (*p).to_string())
                    .collect()
            }),
            hook: overrides
                .notify
                .notification_hook
                .and_then(normalize_optional_string),
        };
        let limits = LimitsConfig {
            max_running_sessions: overrides.limits.max_running_sessions.unwrap_or(50),
            session_eviction_seconds: overrides
                .limits
                .session_eviction_seconds
                .unwrap_or(15)
                .max(1),
            screen_scrollback_rows: overrides
                .limits
                .screen_scrollback_rows
                .unwrap_or(DEFAULT_SCREEN_SCROLLBACK_ROWS),
            silence_seconds: overrides.limits.silence_seconds.unwrap_or(10).max(1),
            stop_grace_seconds: overrides.limits.stop_grace_seconds.unwrap_or(5).max(1),
            journal_retention_days: overrides.limits.journal_retention_days.unwrap_or(0),
            disk_guard_enabled: overrides.limits.disk_guard_enabled.unwrap_or(false),
            disk_min_free_bytes: overrides
                .limits
                .disk_min_free_bytes
                .unwrap_or(DEFAULT_DISK_MIN_FREE_BYTES),
            disk_warn_free_bytes: overrides
                .limits
                .disk_warn_free_bytes
                .unwrap_or(DEFAULT_DISK_WARN_FREE_BYTES),
            disk_sample_interval_seconds: overrides
                .limits
                .disk_sample_interval_seconds
                .unwrap_or(30)
                .max(5),
            disk_sample_max_age_seconds: overrides
                .limits
                .disk_sample_max_age_seconds
                .unwrap_or(120)
                .max(5),
        };
        let web_push = WebPushConfig {
            subject: overrides
                .web_push
                .web_push_subject
                .and_then(normalize_optional_string),
            vapid_public_key: overrides
                .web_push
                .web_push_vapid_public_key
                .and_then(normalize_optional_string),
            vapid_private_key: overrides
                .web_push
                .web_push_vapid_private_key
                .and_then(normalize_optional_string),
            proxy: resolve_optional_string_setting(
                None,
                std::env::var("OLY_WEB_PUSH_PROXY").ok(),
                overrides.web_push.web_push_proxy,
            ),
        };
        let log_level = overrides
            .log_level
            .and_then(normalize_optional_string)
            .unwrap_or_else(|| "info".to_string());

        Self {
            paths,
            http,
            notify,
            limits,
            web_push,
            resume: ResumeConfig {
                patterns: resume_patterns,
            },
            log_level,
            trusted_apps_enabled: overrides.trusted_apps_enabled.unwrap_or(false),
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
        let overrides = try_load_overrides(&self.paths.state_dir)?;
        let mut next = Self::resolve(self.paths.state_dir.clone(), overrides);
        next.runtime_overrides = self.runtime_overrides.clone();
        next.apply_runtime_overrides();
        next.validate()?;
        Ok(next)
    }

    fn validate(&self) -> std::result::Result<(), String> {
        self.http
            .bind
            .parse::<std::net::IpAddr>()
            .map_err(|_| "http_bind must be an IPv4 or IPv6 address".to_string())?;
        if self.http.port == 0 {
            return Err("http_port must be 1..65535 (ephemeral listeners are unsupported)".into());
        }
        if self.limits.max_running_sessions > 1000 {
            return Err("max_running_sessions must be 0..1000 (0 prevents new sessions)".into());
        }
        if self.limits.screen_scrollback_rows > 100_000 {
            return Err("screen_scrollback_rows must be 0..100000".into());
        }
        for (key, value) in [
            (
                "notification_min_interval_seconds",
                self.notify.min_interval_seconds,
            ),
            (
                "session_eviction_seconds",
                self.limits.session_eviction_seconds,
            ),
            ("silence_seconds", self.limits.silence_seconds),
            ("stop_grace_seconds", self.limits.stop_grace_seconds),
        ] {
            if value > 31_536_000 {
                return Err(format!("{key} must not exceed 31536000 seconds (365 days)"));
            }
        }
        if self.limits.journal_retention_days > 36_500 {
            return Err("journal_retention_days must be 0..36500".into());
        }
        if self.limits.disk_warn_free_bytes < self.limits.disk_min_free_bytes {
            return Err(
                "disk_warn_free_bytes must be >= disk_min_free_bytes (warn before refusing)".into(),
            );
        }
        if self.limits.disk_sample_max_age_seconds < self.limits.disk_sample_interval_seconds {
            return Err(
                "disk_sample_max_age_seconds must be >= disk_sample_interval_seconds \
                 (a fresh sample must survive at least one sampling interval)"
                    .into(),
            );
        }
        if self.limits.disk_sample_interval_seconds > 86_400
            || self.limits.disk_sample_max_age_seconds > 86_400
        {
            return Err("disk sample interval/max age must not exceed 86400 seconds".into());
        }
        for (index, pattern) in self.notify.prompt_patterns.iter().enumerate() {
            regex::Regex::new(pattern)
                .map_err(|err| format!("prompt pattern {index}: invalid regex: {err}"))?;
        }
        self.validate_resume_patterns()
    }

    fn validate_resume_patterns(&self) -> std::result::Result<(), String> {
        for (index, rule) in self.resume.patterns.iter().enumerate() {
            if rule.program.trim().is_empty() || rule.command.trim().is_empty() {
                return Err(format!(
                    "resume pattern {index}: program and command must not be empty"
                ));
            }
            let regex = regex::Regex::new(&rule.pattern)
                .map_err(|err| format!("resume pattern {index}: invalid regex: {err}"))?;
            if regex.captures_len() < 2 {
                return Err(format!(
                    "resume pattern {index}: regex must have a capture group"
                ));
            }
        }
        Ok(())
    }

    /// Names of hot-reloadable fields that differ between `self` and `other`.
    ///
    /// Everything listed here is picked up by the running daemon without a
    /// restart; keep this in sync with the reload task in
    /// `daemon::reload` and the live readers (notification monitor, session
    /// start paths, HTTP handlers). The diff is a fixed composition of
    /// per-sub-struct diffs: any future field added to the relevant
    /// sub-structs gets its diff arm automatically.
    pub fn hot_reload_changes(&self, other: &Self) -> Vec<&'static str> {
        let mut changed = Vec::new();
        changed.extend(self.notify.diff(&other.notify));
        changed.extend(self.limits.diff(&other.limits));
        changed.extend(self.web_push.diff(&other.web_push));
        changed.extend(self.resume.diff(&other.resume));
        if self.log_level != other.log_level {
            changed.push("log_level");
        }
        if self.trusted_apps_enabled != other.trusted_apps_enabled {
            changed.push("trusted_apps_enabled");
        }
        changed
    }

    /// Names of fields that differ but only take effect after a daemon
    /// restart (bind address, HTTP port). Paths and socket name silently
    /// stay fixed at the start-up value: the daemon doesn't rebind its
    /// database connection or unix socket mid-flight.
    pub fn restart_required_changes(&self, other: &Self) -> Vec<&'static str> {
        let mut changed = Vec::new();
        changed.extend(self.http.diff(&other.http));
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
            self.http.bind = http_bind.clone();
        }
        if let Some(http_port) = self.runtime_overrides.http_port {
            self.http.port = http_port;
        }
        if let Some(notification_hook) = &self.runtime_overrides.notification_hook {
            self.notify.hook = Some(notification_hook.clone());
        }
        if let Some(web_push_proxy) = &self.runtime_overrides.web_push_proxy {
            self.web_push.proxy = Some(web_push_proxy.clone());
        }
    }

    pub fn wwwroot_dir(&self) -> PathBuf {
        self.paths.state_dir.join("wwwroot")
    }
}

// ---------------------------------------------------------------------------
// Default config generation
// ---------------------------------------------------------------------------

/// Encode raw bytes as base64url without padding.
fn base64url_no_pad(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
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
    use p256::elliptic_curve::sec1::ToSec1Point as _;
    use rand::Rng as _;

    // Retry until we land on a valid scalar (astronomically unlikely to loop more than once).
    let secret = loop {
        let mut key_bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut key_bytes);
        let fb = p256::elliptic_curve::FieldBytes::<p256::NistP256>::from(key_bytes);
        if let Ok(sk) = p256::SecretKey::from_bytes(&fb) {
            break sk;
        }
    };
    let private_b64 = base64url_no_pad(secret.to_bytes().as_ref());
    let public_b64 = base64url_no_pad(secret.public_key().to_sec1_point(false).as_bytes());
    (private_b64, public_b64)
}

/// Linux abstract sockets share the network namespace, not the filesystem or
/// PID namespace. WSL distributions can therefore see one another's sockets
/// even when their state directories have identical absolute paths.
fn default_socket_name(state_dir: &Path) -> String {
    #[cfg(target_os = "linux")]
    {
        scoped_socket_name(state_dir, std::env::var_os("WSL_DISTRO_NAME").as_deref())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = state_dir;
        "open-relay.oly.sock".to_string()
    }
}

#[cfg(target_os = "linux")]
fn scoped_socket_name(state_dir: &Path, distro: Option<&std::ffi::OsStr>) -> String {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;

    let path = std::fs::canonicalize(state_dir).unwrap_or_else(|_| {
        if state_dir.is_absolute() {
            state_dir.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(state_dir)
        }
    });
    let mut hash = Sha256::new();
    hash.update(b"oly-linux-ipc-v1\0");
    hash.update(path.as_os_str().as_bytes());
    hash.update(b"\0");
    if let Some(distro) = distro {
        hash.update(distro.as_bytes());
    }
    format!(
        "open-relay.oly.{}.sock",
        URL_SAFE_NO_PAD.encode(hash.finalize())
    )
}

/// Generate a missing config without overwriting another process's file.
/// Existing unreadable/malformed files are diagnosed by strict loading below.
fn ensure_config_file(state_dir: &Path) -> Result<()> {
    use std::io::Write as _;
    let path = state_dir.join("config.json");
    if path.try_exists()? {
        return Ok(());
    }
    std::fs::create_dir_all(state_dir)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let staging = state_dir.join(format!(".config-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = options.open(&staging)?;
    let (private_key, public_key) = generate_vapid_keypair();
    let contents = serde_json::json!({
        "web_push_vapid_public_key": public_key,
        "web_push_vapid_private_key": private_key,
        "web_push_subject": "mailto:admin@oly.com"
    });
    let result = (|| -> Result<()> {
        file.write_all(serde_json::to_string_pretty(&contents)?.as_bytes())?;
        file.sync_all()?;
        drop(file);
        // An atomic no-clobber hard link publishes only complete JSON. Rename
        // can overwrite another creator's file on Unix; create_new exposes an
        // empty/incomplete destination to concurrent strict readers.
        match std::fs::hard_link(&staging, &path) {
            Ok(()) => eprintln!("info: generated default config at {}", path.display()),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.into()),
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&staging);
    result
}

/// Strict override loading shared by startup and hot reload. Removed recording
/// cap keys are errors, not silently ignored promises of bounded disk use.
fn try_load_overrides(state_dir: &Path) -> std::result::Result<AppConfigOverrides, String> {
    let path = state_dir.join("config.json");
    let raw = std::fs::read_to_string(&path)
        .map_err(|err| format!("could not read {}: {err}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|err| format!("could not parse {}: {err}", path.display()))?;
    if let Some(fields) = value.as_object() {
        for key in [
            "max_journal_bytes_per_session",
            "max_output_log_bytes",
            "max_journal_bytes_per_session_legacy",
        ] {
            if fields.contains_key(key) {
                return Err(format!(
                    "{}: {key} was removed: live journal byte caps cannot bound a current incarnation; remove this key and use journal_retention_days for stopped sessions or an OS disk quota (see the migration guide)",
                    path.display()
                ));
            }
        }
    }
    let overrides = serde_json::from_value::<AppConfigOverrides>(value)
        .map_err(|err| format!("could not parse {}: {err}", path.display()))?;
    for key in overrides.unknown.keys() {
        eprintln!(
            "warning: {}: unknown config key '{key}' ignored; config keys must be flat (see oly skill --daemon)",
            path.display()
        );
    }
    Ok(overrides)
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
            paths: super::PathsConfig {
                state_dir: state_dir.clone(),
                sessions_dir: state_dir.join("sessions"),
                db_file: state_dir.join("oly.db"),
                lock_file: state_dir.join("daemon.lock"),
                info_file: state_dir.join("daemon.info"),
                socket_name: "test.sock".to_string(),
                socket_file: state_dir.join("daemon.sock"),
            },
            http: super::HttpConfig {
                bind: "127.0.0.1".to_string(),
                port: 15443,
            },
            notify: super::NotifyConfig {
                min_interval_seconds: 10,
                prompt_patterns: Vec::new(),
                hook: Some("config-hook".to_string()),
            },
            limits: super::LimitsConfig {
                max_running_sessions: 50,
                session_eviction_seconds: 15,
                screen_scrollback_rows: super::DEFAULT_SCREEN_SCROLLBACK_ROWS,
                silence_seconds: 10,
                stop_grace_seconds: 5,
                journal_retention_days: 0,
                disk_guard_enabled: false,
                disk_min_free_bytes: super::DEFAULT_DISK_MIN_FREE_BYTES,
                disk_warn_free_bytes: super::DEFAULT_DISK_WARN_FREE_BYTES,
                disk_sample_interval_seconds: 30,
                disk_sample_max_age_seconds: 120,
            },
            web_push: super::WebPushConfig {
                subject: None,
                vapid_public_key: None,
                vapid_private_key: None,
                proxy: Some("http://config-proxy:8080".to_string()),
            },
            resume: crate::config::ResumeConfig::default(),
            log_level: "info".to_string(),
            trusted_apps_enabled: false,
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

        assert_eq!(config.http.bind, "0.0.0.0");
        assert_eq!(config.http.port, 17000);
        assert_eq!(config.notify.hook.as_deref(), Some("C:/tools/notify.exe"));
        assert_eq!(
            config.web_push.proxy.as_deref(),
            Some("socks5://127.0.0.1:1080")
        );
    }

    #[test]
    fn runtime_overrides_leave_config_values_when_not_provided() {
        let config = test_config().with_runtime_overrides(None, None, None, None);

        assert_eq!(config.http.bind, "127.0.0.1");
        assert_eq!(config.http.port, 15443);
        assert_eq!(config.notify.hook.as_deref(), Some("config-hook"));
        assert_eq!(
            config.web_push.proxy.as_deref(),
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
        // Sanity: the per-axis fields are wrapped in sub-structs; the JSON
        // shape is preserved via `#[serde(flatten)]`, so the wire-level
        // override name is unchanged.
        let overrides: super::AppConfigOverrides =
            serde_json::from_str(r#"{"screen_scrollback_rows": 250}"#).expect("parse override");
        assert_eq!(overrides.limits.screen_scrollback_rows, Some(250));

        let empty: super::AppConfigOverrides = serde_json::from_str("{}").expect("parse empty");
        assert_eq!(empty.limits.screen_scrollback_rows, None);
    }

    #[test]
    fn resume_patterns_are_the_only_source_and_can_be_disabled() {
        let custom = r#"{"program":"agent","pattern":"agent --resume ([a-z0-9-]+)","command":"agent --restore $1"}"#;
        let state_dir = PathBuf::from("test-state");

        // Absent key: detection is disabled; there are no built-in rules.
        let unset = AppConfig::resolve(state_dir.clone(), Default::default());
        assert!(unset.resume.patterns.is_empty());

        let configured: super::AppConfigOverrides =
            serde_json::from_str(&format!(r#"{{"resume_patterns":[{custom}]}}"#))
                .expect("parse matcher");
        let configured = AppConfig::resolve(state_dir.clone(), configured);
        assert_eq!(configured.resume.patterns.len(), 1);
        assert_eq!(configured.resume.patterns[0].program, "agent");
        assert_eq!(unset.hot_reload_changes(&configured), vec!["patterns"]);

        let disabled: super::AppConfigOverrides =
            serde_json::from_str(r#"{"resume_patterns":[]}"#).expect("parse empty matchers");
        assert!(
            AppConfig::resolve(state_dir, disabled)
                .resume
                .patterns
                .is_empty()
        );
    }

    #[test]
    fn invalid_resume_regex_rejects_hot_reload_without_losing_config() {
        let state_dir =
            std::env::temp_dir().join(format!("oly_config_resume_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        let config_path = state_dir.join("config.json");
        std::fs::write(
            &config_path,
            r#"{"resume_patterns":[{"program":"agent","pattern":"(","command":"agent $1"}]}"#,
        )
        .expect("write config");
        let mut config = test_config();
        config.paths.state_dir = state_dir.clone();
        assert!(config.try_reload().unwrap_err().contains("invalid regex"));
        std::fs::write(&config_path, r#"{"resume_patterns":[]}"#).expect("rewrite config");
        assert!(
            config
                .try_reload()
                .expect("valid reload")
                .resume
                .patterns
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// Legacy single-word `bind` JSON key must keep loading alongside
    /// the canonical `http_bind`; otherwise every pre-flatten
    /// `config.json` would fail with `unknown field`.
    #[test]
    fn http_bind_alias_accepts_legacy_bind_key() {
        let overrides: super::AppConfigOverrides =
            serde_json::from_str(r#"{"bind": "10.0.0.1"}"#).expect("parse legacy key");
        assert_eq!(overrides.http.http_bind.as_deref(), Some("10.0.0.1"));

        let canonical: super::AppConfigOverrides =
            serde_json::from_str(r#"{"http_bind": "10.0.0.2"}"#).expect("parse canonical key");
        assert_eq!(canonical.http.http_bind.as_deref(), Some("10.0.0.2"));
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
        config.paths.state_dir = state_dir.clone();

        let reloaded = config.try_reload().expect("reload should succeed");
        assert_eq!(reloaded.notify.hook.as_deref(), Some("old-hook"));
        assert_eq!(reloaded.limits.silence_seconds, 42);
        // Fields absent from the file fall back to defaults, not to the
        // previous in-memory values.
        assert_eq!(reloaded.limits.max_running_sessions, 50);

        std::fs::write(
            state_dir.join("config.json"),
            r#"{"notification_hook": "new-hook", "max_running_sessions": 7}"#,
        )
        .expect("rewrite config.json");

        let reloaded = config.try_reload().expect("second reload should succeed");
        assert_eq!(reloaded.notify.hook.as_deref(), Some("new-hook"));
        assert_eq!(reloaded.limits.max_running_sessions, 7);
        assert_eq!(reloaded.limits.silence_seconds, 10);

        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn notification_min_interval_is_configurable_and_hot_reloadable() {
        let mut base = test_config();
        base.notify.min_interval_seconds = 10;
        let mut changed = base.clone();
        changed.notify.min_interval_seconds = 30;
        assert!(
            base.hot_reload_changes(&changed)
                .contains(&"min_interval_seconds"),
            "changing the notify cooldown should be reported as a hot-reloadable change"
        );

        let state_dir = std::env::temp_dir().join(format!(
            "oly_config_notify_interval_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        std::fs::write(
            state_dir.join("config.json"),
            r#"{"notification_min_interval_seconds": 45}"#,
        )
        .expect("write config.json");
        let mut config = test_config();
        config.paths.state_dir = state_dir.clone();
        let reloaded = config.try_reload().expect("reload should succeed");
        assert_eq!(reloaded.notify.min_interval_seconds, 45);

        // Absent from the file → falls back to the default, not the old value.
        std::fs::write(state_dir.join("config.json"), r#"{}"#).expect("rewrite config.json");
        let reloaded = config.try_reload().expect("second reload should succeed");
        assert_eq!(reloaded.notify.min_interval_seconds, 10);

        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn try_reload_rejects_unparseable_config() {
        let state_dir =
            std::env::temp_dir().join(format!("oly_config_reload_bad_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        std::fs::write(state_dir.join("config.json"), "{ not json").expect("write config.json");

        let mut config = test_config();
        config.paths.state_dir = state_dir.clone();

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
        config.paths.state_dir = state_dir.clone();
        let config =
            config.with_runtime_overrides(None, Some(17000), Some("cli-hook".to_string()), None);

        let reloaded = config.try_reload().expect("reload should succeed");
        assert_eq!(
            reloaded.notify.hook.as_deref(),
            Some("cli-hook"),
            "CLI flag must keep winning over the edited config file"
        );
        assert_eq!(reloaded.http.port, 17000);

        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn startup_and_reload_share_validation_without_rewriting_existing_files() {
        let state_dir =
            std::env::temp_dir().join(format!("oly_config_validation_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&state_dir).unwrap();
        let mut current = test_config();
        current.paths.state_dir = state_dir.clone();
        for (text, expected) in [
            ("{ broken", "could not parse"),
            (r#"{"http_bind":"hostname"}"#, "http_bind"),
            (r#"{"http_port":0}"#, "http_port"),
            (r#"{"max_running_sessions":1001}"#, "max_running_sessions"),
            (
                r#"{"screen_scrollback_rows":100001}"#,
                "screen_scrollback_rows",
            ),
            (
                r#"{"silence_seconds":18446744073709551615}"#,
                "silence_seconds",
            ),
            (
                r#"{"journal_retention_days":4294967295}"#,
                "journal_retention_days",
            ),
            (r#"{"prompt_patterns":["("]}"#, "prompt pattern"),
        ] {
            std::fs::write(state_dir.join("config.json"), text).unwrap();
            assert!(
                AppConfig::load_from_state_dir(state_dir.clone())
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
            assert!(current.try_reload().unwrap_err().contains(expected));
            assert_eq!(
                std::fs::read_to_string(state_dir.join("config.json")).unwrap(),
                text
            );
        }
        std::fs::write(
            state_dir.join("config.json"),
            r#"{"max_running_sessions":0,"screen_scrollback_rows":0,"prompt_patterns":[]}"#,
        )
        .unwrap();
        assert!(AppConfig::load_from_state_dir(state_dir.clone()).is_ok());
        std::fs::remove_file(state_dir.join("config.json")).unwrap();
        assert!(AppConfig::load_from_state_dir(state_dir.clone()).is_ok());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(state_dir.join("config.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(state_dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_socket_names_are_scoped_by_state_and_wsl_distribution() {
        use std::ffi::OsStr;
        let first = PathBuf::from("/tmp/oly-first-state");
        let second = PathBuf::from("/tmp/oly-second-state");
        let debian = Some(OsStr::new("Debian"));
        let ubuntu = Some(OsStr::new("Ubuntu"));
        let name = super::scoped_socket_name(&first, debian);
        assert_eq!(name, super::scoped_socket_name(&first, debian));
        assert_ne!(name, super::scoped_socket_name(&second, debian));
        assert_ne!(name, super::scoped_socket_name(&first, ubuntu));
        assert_ne!(name, super::scoped_socket_name(&first, None));
        assert!(name.len() < 108, "must fit a Linux abstract socket name");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_socket_scope_canonicalizes_relative_paths_and_symlinks() {
        let dir = std::env::temp_dir().join(format!("oly_socket_scope_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("state")).unwrap();
        std::os::unix::fs::symlink(dir.join("state"), dir.join("alias")).unwrap();
        assert_eq!(
            super::scoped_socket_name(&dir.join("state"), None),
            super::scoped_socket_name(&dir.join("alias"), None)
        );
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            super::scoped_socket_name(std::path::Path::new("."), None),
            super::scoped_socket_name(&cwd, None)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_first_loads_never_observe_partial_config() {
        let dir =
            std::env::temp_dir().join(format!("oly_config_first_load_{}", uuid::Uuid::new_v4()));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let threads = (0..16)
            .map(|_| {
                let dir = dir.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    AppConfig::load_from_state_dir(dir).unwrap()
                })
            })
            .collect::<Vec<_>>();
        let configs = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect::<Vec<_>>();
        assert!(
            configs
                .iter()
                .all(|c| c.web_push.vapid_public_key == configs[0].web_push.vapid_public_key)
        );
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "staging files cleaned up"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trusted_apps_are_opt_in_and_reloadable() {
        let base = AppConfig::resolve(PathBuf::from("test-state"), Default::default());
        assert!(!base.trusted_apps_enabled);
        let overrides = serde_json::from_str(r#"{"trusted_apps_enabled":true}"#).unwrap();
        let enabled = AppConfig::resolve(PathBuf::from("test-state"), overrides);
        assert!(enabled.trusted_apps_enabled);
        assert_eq!(
            base.hot_reload_changes(&enabled),
            vec!["trusted_apps_enabled"]
        );
    }

    #[test]
    fn unknown_keys_are_collected_without_misclassifying_known_flat_fields() {
        let overrides: super::AppConfigOverrides = serde_json::from_str(r#"{"bind":"127.0.0.1","http_port":17000,"notification_hook":"echo done","web_push_subject":"mailto:test@example.invalid","journal_retention_days":1,"resume_patterns":[],"notification_hookk":"typo","limits":{"max_running_sessions":2}}"#).unwrap();
        assert_eq!(
            overrides
                .unknown
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["limits", "notification_hookk"]
        );
        assert_eq!(overrides.http.http_port, Some(17000));
    }

    #[test]
    fn removed_byte_cap_keys_fail_explicitly_on_load_and_reload() {
        let state_dir =
            std::env::temp_dir().join(format!("oly_removed_caps_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&state_dir).unwrap();
        let mut config = test_config();
        config.paths.state_dir = state_dir.clone();
        for key in [
            "max_journal_bytes_per_session",
            "max_output_log_bytes",
            "max_journal_bytes_per_session_legacy",
        ] {
            std::fs::write(state_dir.join("config.json"), format!("{{\"{key}\":0}}")).unwrap();
            let error = super::try_load_overrides(&state_dir).unwrap_err();
            assert!(error.contains(key) && error.contains("was removed"));
            assert!(config.try_reload().unwrap_err().contains("was removed"));
        }
        std::fs::write(
            state_dir.join("config.json"),
            r#"{"journal_retention_days":30}"#,
        )
        .unwrap();
        assert_eq!(
            config.try_reload().unwrap().limits.journal_retention_days,
            30
        );
        std::fs::remove_dir_all(state_dir).unwrap();
    }

    #[test]
    fn hot_reload_changes_lists_only_hot_fields() {
        let base = test_config();
        let mut changed = base.clone();
        changed.notify.hook = Some("other-hook".to_string());
        changed.http.port = 1;

        // `notify.hook` is hot-reloadable (rebuilds the notification
        // pipeline immediately); `http.port` is restart-only (the bound
        // socket can't move under load).  The diff names are the flat
        // rust field names of the resolved sub-structs.
        assert_eq!(base.hot_reload_changes(&changed), vec!["hook"]);
        assert_eq!(base.restart_required_changes(&changed), vec!["port"]);
    }
}
