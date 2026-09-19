use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};
use tracing::{debug, info, warn};

use crate::http::AppState;

const MAX_FAILED_ATTEMPTS: u32 = 3;
const LOCKOUT_DURATION: Duration = Duration::from_secs(15 * 60); // 15 minutes
/// How often the background task sweeps the lockout and session tables for
/// expired entries.
const LOCKOUT_CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60); // 5 minutes
/// Browser session lifetime (ADR-0007): sessions are random, server-side,
/// revocable, and expiring. The cookie mirrors this TTL.
pub const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60); // 7 days
const AUTH_COOKIE_NAME: &str = "oly_auth_token";
/// How long a verified API-key → scopes mapping is cached, so per-request
/// Argon2 verification stays off the hot path. Revoking a key takes effect
/// within this window.
const API_KEY_CACHE_TTL: Duration = Duration::from_secs(60);

// ── Authorization scopes (ADR-0007, M5-4) ────────────────────────────────────

/// Read-only access: lists, session details, logs, SSE events, node list.
pub const SCOPE_OBSERVE: &str = "observe";
/// Interactive access: attach streams and session input/upload.
pub const SCOPE_CONTROL: &str = "control";
/// Administrative access: create/stop/kill sessions, metadata, notifications,
/// push subscriptions, API-key management.
pub const SCOPE_MANAGE: &str = "manage";
/// Federation access: secondary node join.
pub const SCOPE_NODE: &str = "node";
/// Wildcard scope granted by legacy keys created before scopes existed.
pub const SCOPE_ALL: &str = "all";

const KNOWN_SCOPES: [&str; 5] = [
    SCOPE_OBSERVE,
    SCOPE_CONTROL,
    SCOPE_MANAGE,
    SCOPE_NODE,
    SCOPE_ALL,
];

/// Validate a comma-separated scope list from the CLI. Returns an error
/// naming the first unknown scope.
pub fn validate_scope_list(scopes: &str) -> std::result::Result<(), String> {
    if scopes.trim().is_empty() {
        return Err("scope list must not be empty".to_string());
    }
    for scope in scopes.split(',').map(str::trim) {
        if !KNOWN_SCOPES.contains(&scope) {
            return Err(format!(
                "unknown scope '{scope}' (known: {})",
                KNOWN_SCOPES.join(", ")
            ));
        }
    }
    Ok(())
}

/// Does the stored scope list grant `required`?
pub fn scopes_allow(stored: &str, required: &str) -> bool {
    stored
        .split(',')
        .map(str::trim)
        .any(|scope| scope == required || scope == SCOPE_ALL)
}

/// Classify the scope a protected `/api/*` route requires.
pub fn required_scope_for(method: &axum::http::Method, path: &str) -> &'static str {
    if method == axum::http::Method::GET {
        // The attach endpoint is a GET WebSocket upgrade but grants
        // interactive control — classify by capability, not verb.
        if path.ends_with("/attach") {
            return SCOPE_CONTROL;
        }
        return SCOPE_OBSERVE;
    }
    // Mutating routes: input/upload touch a live child; the rest are
    // lifecycle/admin operations.
    if path.ends_with("/input") || path.ends_with("/upload") {
        SCOPE_CONTROL
    } else {
        SCOPE_MANAGE
    }
}

// ── AuthState ────────────────────────────────────────────────────────────────

pub struct AuthState {
    password_hash: String,
    /// Server-side session registry (ADR-0007): random token → expiry.
    /// Sessions are revocable (removal) and expiring; a daemon restart
    /// invalidates every session, requiring re-login.
    sessions: Mutex<HashMap<String, Instant>>,
    /// Epoch bumped on every revocation so open control streams (WebSocket
    /// attach, SSE) can re-validate their token and close loudly.
    revocations: watch::Sender<u64>,
    /// Cache of verified API keys: sha256(presented key) hex → (scopes,
    /// verified-at). Bounds per-request Argon2 verification cost.
    api_key_cache: Mutex<HashMap<String, (String, Instant)>>,
    /// Per-IP lockout table. Each client is tracked independently so that a
    /// brute-force attempt from one IP cannot lock out legitimate users.
    lockout: Mutex<HashMap<IpAddr, LockoutRecord>>,
}

#[derive(Default)]
struct LockoutRecord {
    failed_attempts: u32,
    locked_until: Option<Instant>,
}

pub(crate) enum FailureOutcome {
    LockedOut { until: Instant },
    AttemptsRemaining(u32),
}

impl AuthState {
    pub fn new(password_hash: String) -> Arc<Self> {
        let (revocations, _) = watch::channel(0u64);
        let state = Arc::new(Self {
            password_hash,
            sessions: Mutex::new(HashMap::new()),
            revocations,
            api_key_cache: Mutex::new(HashMap::new()),
            lockout: Mutex::new(HashMap::new()),
        });
        state.spawn_cleanup_task();
        state
    }

    /// Spawn a background task that periodically evicts expired lockout records
    /// so the in-memory map stays bounded even under sustained attack traffic.
    fn spawn_cleanup_task(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(LOCKOUT_CLEANUP_INTERVAL);
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                let Some(state) = weak.upgrade() else {
                    // AuthState has been dropped; stop the task.
                    break;
                };
                let now = Instant::now();
                let mut lockout = state.lockout.lock().await;
                let before = lockout.len();
                lockout.retain(|_, r| r.locked_until.is_none_or(|t| now < t));
                let removed = before - lockout.len();
                if removed > 0 {
                    debug!(
                        removed,
                        "auth: background cleanup evicted expired lockout records"
                    );
                }
                drop(lockout);
                let mut sessions = state.sessions.lock().await;
                let before = sessions.len();
                sessions.retain(|_, expires_at| now < *expires_at);
                let evicted = before - sessions.len();
                if evicted > 0 {
                    debug!(evicted, "auth: background cleanup evicted expired sessions");
                }
                drop(sessions);
                let mut cache = state.api_key_cache.lock().await;
                cache.retain(|_, (_, verified_at)| now < *verified_at + API_KEY_CACHE_TTL);
            }
        });
    }

    /// Issue a new random, expiring session token (ADR-0007).
    pub async fn issue_session(&self) -> String {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        self.sessions
            .lock()
            .await
            .insert(token.clone(), Instant::now() + SESSION_TTL);
        token
    }

    /// Validate a session token against the server-side registry. Expired
    /// sessions are evicted on observation and fail validation.
    pub async fn validate_token(&self, token: &str) -> bool {
        let mut sessions = self.sessions.lock().await;
        match sessions.get(token) {
            Some(expires_at) if Instant::now() < *expires_at => true,
            Some(_) => {
                sessions.remove(token);
                false
            }
            None => false,
        }
    }

    /// Revoke a session token and notify open streams via the revocation
    /// epoch so they re-validate and close loudly.
    pub async fn revoke_token(&self, token: &str) {
        if self.sessions.lock().await.remove(token).is_some() {
            let epoch = *self.revocations.borrow() + 1;
            let _ = self.revocations.send(epoch);
        }
    }

    /// Watch handle for the revocation epoch (open streams select on it).
    pub fn revocation_watch(&self) -> watch::Receiver<u64> {
        self.revocations.subscribe()
    }

    /// Verify a presented API key against stored (hash, scopes) entries,
    /// returning the granted scopes. Results are cached briefly so the hot
    /// path does not pay Argon2 verification per request.
    pub async fn verify_api_key_scopes(
        &self,
        presented: &str,
        entries: &[(String, String)],
    ) -> Option<String> {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(presented.as_bytes());
        let cache_key: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        {
            let cache = self.api_key_cache.lock().await;
            if let Some((scopes, verified_at)) = cache.get(&cache_key)
                && Instant::now() < *verified_at + API_KEY_CACHE_TTL
            {
                return Some(scopes.clone());
            }
        }
        let presented = presented.to_string();
        let entries = entries.to_vec();
        let matched = tokio::task::spawn_blocking(move || {
            entries
                .into_iter()
                .find(|(hash, _)| verify_api_key_hash(&presented, hash))
                .map(|(_, scopes)| scopes)
        })
        .await
        .ok()
        .flatten()?;
        self.api_key_cache
            .lock()
            .await
            .insert(cache_key, (matched.clone(), Instant::now()));
        Some(matched)
    }

    /// Returns `Some(locked_until)` if the IP is currently locked out.
    /// Evicts the record and returns `None` if the lockout has expired.
    pub(crate) async fn locked_until(&self, ip: IpAddr) -> Option<Instant> {
        let mut lockout = self.lockout.lock().await;
        let record = lockout.get(&ip)?;
        let t = record.locked_until?;
        if Instant::now() < t {
            Some(t)
        } else {
            lockout.remove(&ip);
            None
        }
    }

    /// Record a failed login attempt. Returns whether the IP is now locked out
    /// or how many attempts remain before lockout.
    pub(crate) async fn record_failure(&self, ip: IpAddr) -> FailureOutcome {
        let mut lockout = self.lockout.lock().await;
        let record = lockout.entry(ip).or_default();
        record.failed_attempts += 1;
        if record.failed_attempts >= MAX_FAILED_ATTEMPTS {
            let until = Instant::now() + LOCKOUT_DURATION;
            record.locked_until = Some(until);
            FailureOutcome::LockedOut { until }
        } else {
            FailureOutcome::AttemptsRemaining(MAX_FAILED_ATTEMPTS - record.failed_attempts)
        }
    }
}

// ── Public Hash Helper ──────────────────────────────────────────────────────

/// Hash a plaintext password with Argon2id. Returns a PHC-format string.
pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
    let salt = SaltString::generate(&mut rand::thread_rng());
    let argon2 = Argon2::default();
    Ok(argon2
        .hash_password(password.as_bytes(), &salt)?
        .to_string())
}

/// Verify a plaintext API key against a stored Argon2id hash.
pub(crate) fn verify_api_key_hash(key: &str, hash: &str) -> bool {
    use argon2::{Argon2, PasswordHash, PasswordVerifier};
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(key.as_bytes(), &parsed)
        .is_ok()
}

// ── Request / Response DTOs ──────────────────────────────────────────────────

#[derive(Serialize)]
pub struct AuthStatusResponse {
    pub auth_required: bool,
}

#[derive(Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    pub token: String,
}

// ── IP extraction helper ─────────────────────────────────────────────────────

/// Return the effective client IP from headers + peer socket address.
///
/// Only trusts `X-Real-IP` / `X-Forwarded-For` when the direct peer address
/// is a loopback IP (i.e. the request came through a local reverse proxy).
/// This prevents remote attackers from spoofing arbitrary client IPs by
/// setting these headers directly.
pub(super) fn effective_ip(headers: &HeaderMap, peer: IpAddr) -> IpAddr {
    if !peer.is_loopback() {
        return peer;
    }

    headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok())
        .or_else(|| {
            headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.trim().parse().ok())
        })
        .unwrap_or(peer)
}

// ── Axum Handlers ────────────────────────────────────────────────────────────

/// GET /api/auth/status — always public, no auth required.
pub async fn status(State(state): State<AppState>) -> impl IntoResponse {
    Json(AuthStatusResponse {
        auth_required: state.auth.is_some(),
    })
}

/// POST /api/auth/login — verify password, return a session token.
/// Rate-limited **per client IP**: 3 failed attempts → 15-minute lockout.
/// Different clients track independently, so attackers cannot lock out
/// legitimate users.
pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<LoginRequest>,
) -> impl IntoResponse {
    let client_ip = effective_ip(&headers, peer.ip());

    let Some(auth) = &state.auth else {
        // No auth configured; treat as always-authenticated.
        debug!(ip = %client_ip, "auth: login called in no-auth mode");
        return (
            StatusCode::OK,
            [(axum::http::header::SET_COOKIE, clear_auth_cookie())],
            Json(serde_json::json!({ "token": "" })),
        )
            .into_response();
    };

    info!(ip = %client_ip, "auth: login attempt");

    // ── Check per-IP lockout ─────────────────────────────────────────────────
    if let Some(locked_until) = auth.locked_until(client_ip).await {
        let secs = locked_until.duration_since(Instant::now()).as_secs();
        warn!(ip = %client_ip, retry_after_seconds = secs, "auth: login rejected — client is locked out");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", secs.to_string())],
            Json(serde_json::json!({
                "error": "too_many_attempts",
                "retry_after_seconds": secs
            })),
        )
            .into_response();
    }

    // ── Verify password (blocking – Argon2 is CPU-intensive) ─────────────────
    let hash = auth.password_hash.clone();
    let password = payload.password.clone();
    let verified = tokio::task::spawn_blocking(move || {
        use argon2::{Argon2, PasswordHash, PasswordVerifier};
        let parsed = PasswordHash::new(&hash).ok()?;
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .ok()?;
        Some(())
    })
    .await
    .ok()
    .flatten();

    if verified.is_some() {
        auth.lockout.lock().await.remove(&client_ip);
        let token = auth.issue_session().await;
        info!(ip = %client_ip, "auth: login success — session token issued");
        let secure = request_is_tls(&headers);
        return (
            StatusCode::OK,
            [(
                axum::http::header::SET_COOKIE,
                build_auth_cookie(&token, secure),
            )],
            Json(LoginResponse { token }),
        )
            .into_response();
    }

    // ── Failed attempt ───────────────────────────────────────────────────────
    match auth.record_failure(client_ip).await {
        FailureOutcome::LockedOut { until } => {
            let secs = until.duration_since(Instant::now()).as_secs();
            warn!(
                ip = %client_ip,
                lockout_minutes = LOCKOUT_DURATION.as_secs() / 60,
                "auth: client locked out after too many failed attempts"
            );
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("Retry-After", secs.to_string())],
                Json(serde_json::json!({
                    "error": "too_many_attempts",
                    "retry_after_seconds": secs
                })),
            )
                .into_response()
        }
        FailureOutcome::AttemptsRemaining(attempts_remaining) => {
            warn!(ip = %client_ip, attempts_remaining, "auth: invalid password");
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": "invalid_password",
                    "attempts_remaining": attempts_remaining
                })),
            )
                .into_response()
        }
    }
}

/// POST /api/auth/logout — clear the caller's auth cookie.
///
/// Sessions are random and server-side (ADR-0007): logout revokes the
/// caller's token, bumps the revocation epoch so open WebSocket/SSE streams
/// close loudly, and clears the cookie client-side.
pub async fn logout(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let client_ip = effective_ip(&headers, peer.ip());

    if state.auth.is_none() {
        return (
            StatusCode::OK,
            [(axum::http::header::SET_COOKIE, clear_auth_cookie())],
        )
            .into_response();
    }

    if let Some(auth) = &state.auth
        && let Some(token) = extract_request_token_parts(&headers, None)
    {
        auth.revoke_token(&token).await;
    }

    info!(ip = %client_ip, "auth: logout — session revoked, cookie cleared");

    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, clear_auth_cookie())],
    )
        .into_response()
}

// ── Auth Middleware ──────────────────────────────────────────────────────────

/// Axum middleware: enforce Bearer-token authentication for all `/api/` routes
/// except `/api/health` and `/api/auth/*`.
pub async fn require_auth(
    State(state): State<AppState>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request.uri().path().to_owned();

    if path == "/api/health" || path.starts_with("/api/auth/") || !path.starts_with("/api/") {
        return next.run(request).await;
    }

    // Only allow query-string tokens for WebSocket/SSE upgrade endpoints
    // where browser APIs cannot send Authorization headers or cookies.
    let is_upgrade_path = path.ends_with("/attach") || path == "/api/sessions/events";
    let query = if is_upgrade_path {
        request.uri().query()
    } else {
        None
    };
    let token = extract_request_token_parts(request.headers(), query);
    let bearer = extract_bearer_token(request.headers());
    let method = request.method().clone();
    let client_ip = extract_request_client_ip(&request);
    if let Some(response) =
        authorize_request(&state, &method, &path, token, bearer, client_ip).await
    {
        return response;
    }

    next.run(request).await
}

pub(super) async fn authorize_request(
    state: &AppState,
    method: &axum::http::Method,
    path: &str,
    token: Option<String>,
    bearer: Option<String>,
    client_ip: Option<String>,
) -> Option<Response> {
    let auth = state.auth.as_ref().map(Arc::clone)?;

    if let Some(token) = token
        && auth.validate_token(&token).await
    {
        debug!(path = %path, "auth: authorized request (session)");
        return None;
    }

    // Scoped machine credentials (ADR-0007): an API key presented as a
    // Bearer token authorizes routes matching its scope list. Query-string
    // and cookie credentials are never treated as API keys.
    if let Some(key) = bearer {
        match state.db.list_api_key_entries().await {
            Ok(entries) => {
                if let Some(scopes) = auth.verify_api_key_scopes(&key, &entries).await {
                    let required = required_scope_for(method, path);
                    if scopes_allow(&scopes, required) {
                        debug!(path = %path, scope = required, "auth: authorized request (api key)");
                        return None;
                    }
                    warn!(
                        path = %path,
                        required_scope = required,
                        "auth: API key lacks the required scope"
                    );
                    return Some(
                        (
                            StatusCode::FORBIDDEN,
                            Json(serde_json::json!({
                                "error": "insufficient_scope",
                                "required_scope": required,
                            })),
                        )
                            .into_response(),
                    );
                }
            }
            Err(err) => {
                warn!(error = %err, "auth: failed to list API keys");
            }
        }
    }

    let client_ip = client_ip.unwrap_or_else(|| "unknown".to_string());

    warn!(ip = %client_ip, path = %path, "auth: unauthorized request rejected");

    Some(
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "unauthorized" })),
        )
            .into_response(),
    )
}

pub(super) fn extract_request_client_ip(request: &Request) -> Option<String> {
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| effective_ip(request.headers(), ci.0.ip()).to_string())
}

pub(super) fn extract_request_token_parts(
    headers: &HeaderMap,
    query: Option<&str>,
) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_owned())
        .or_else(|| {
            query.and_then(|q| {
                q.split('&')
                    .find(|part| part.starts_with("token="))
                    .and_then(|part| part.strip_prefix("token="))
                    .map(|token| token.to_owned())
            })
        })
        .or_else(|| extract_cookie_token(headers))
}

/// Extract only the Authorization: Bearer credential (never query/cookie),
/// used when deciding whether a request carries machine credentials.
pub(super) fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_owned())
}

/// WebSocket Origin validation (ADR-0007): a browser cross-site WebSocket
/// must not ride the ambient auth cookie. Non-browser clients (no Origin
/// header) are unaffected; an Origin whose host[:port] does not match the
/// Host header is rejected.
pub fn ws_origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(axum::http::header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let authority = origin
        .split("://")
        .nth(1)
        .unwrap_or(origin)
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_ascii_lowercase);
    match host {
        Some(host) => !authority.is_empty() && authority == host,
        None => false,
    }
}

fn extract_cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| {
            raw.split(';').find_map(|cookie| {
                let (name, value) = cookie.trim().split_once('=')?;
                if name == AUTH_COOKIE_NAME && !value.is_empty() {
                    Some(value.to_string())
                } else {
                    None
                }
            })
        })
}

fn build_auth_cookie(token: &str, secure: bool) -> String {
    let secure_flag = if secure { "; Secure" } else { "" };
    let max_age = SESSION_TTL.as_secs();
    format!(
        "{AUTH_COOKIE_NAME}={token}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax{secure_flag}"
    )
}

fn clear_auth_cookie() -> String {
    format!("{AUTH_COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// Returns true when the request appears to have arrived over TLS
/// (via a reverse proxy that sets `X-Forwarded-Proto: https`).
fn request_is_tls(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"))
}

#[cfg(test)]
mod tests {
    use super::{
        AUTH_COOKIE_NAME, AuthState, SCOPE_CONTROL, SCOPE_MANAGE, SCOPE_NODE, SCOPE_OBSERVE,
        SESSION_TTL, build_auth_cookie, clear_auth_cookie, extract_request_token_parts,
        hash_password, required_scope_for, scopes_allow, validate_scope_list, ws_origin_allowed,
    };
    use axum::http::{HeaderMap, Method, header};

    #[tokio::test]
    async fn sessions_are_random_expiring_and_revocable() {
        let state = AuthState::new(hash_password("hunter2").expect("password should hash"));

        let token_a = state.issue_session().await;
        let token_b = state.issue_session().await;
        // Random: two logins never share a token.
        assert_ne!(token_a, token_b);
        assert_eq!(token_a.len(), 64);
        assert!(token_a.bytes().all(|b| b.is_ascii_hexdigit()));

        assert!(state.validate_token(&token_a).await);
        assert!(state.validate_token(&token_b).await);
        assert!(!state.validate_token("not-the-token").await);
        // Prefix of a real token is rejected.
        assert!(!state.validate_token(&token_a[..token_a.len() - 1]).await);

        // Revocation invalidates only the revoked token and bumps the epoch.
        let mut watch = state.revocation_watch();
        let epoch = *watch.borrow();
        state.revoke_token(&token_a).await;
        assert!(!state.validate_token(&token_a).await);
        assert!(state.validate_token(&token_b).await);
        watch.changed().await.expect("revocation epoch must bump");
        assert_eq!(*watch.borrow(), epoch + 1);

        // Re-issuing never collides with a revoked token.
        let token_c = state.issue_session().await;
        assert_ne!(token_a, token_c);
    }

    #[test]
    fn cookie_lifetime_matches_session_ttl() {
        assert_eq!(SESSION_TTL.as_secs(), 7 * 24 * 60 * 60);
        let cookie = build_auth_cookie("abc123", false);
        assert!(cookie.contains("Max-Age=604800"));
    }

    #[test]
    fn scope_lists_parse_and_gate() {
        assert!(validate_scope_list("observe,control").is_ok());
        assert!(validate_scope_list("all").is_ok());
        assert!(validate_scope_list("").is_err());
        assert!(validate_scope_list("observe,root").is_err());

        assert!(scopes_allow("observe,control", SCOPE_OBSERVE));
        assert!(scopes_allow("observe,control", SCOPE_CONTROL));
        assert!(!scopes_allow("observe,control", SCOPE_MANAGE));
        assert!(!scopes_allow("observe,control", SCOPE_NODE));
        assert!(scopes_allow("all", SCOPE_MANAGE));
        assert!(scopes_allow(" node ", SCOPE_NODE));
    }

    #[test]
    fn routes_classify_into_scopes() {
        assert_eq!(
            required_scope_for(&Method::GET, "/api/sessions"),
            SCOPE_OBSERVE
        );
        assert_eq!(
            required_scope_for(&Method::GET, "/api/sessions/abc/logs"),
            SCOPE_OBSERVE
        );
        // Attach is a GET upgrade but grants interactive control.
        assert_eq!(
            required_scope_for(&Method::GET, "/api/sessions/abc/attach"),
            SCOPE_CONTROL
        );
        assert_eq!(
            required_scope_for(&Method::POST, "/api/sessions/abc/input"),
            SCOPE_CONTROL
        );
        assert_eq!(
            required_scope_for(&Method::POST, "/api/sessions/abc/upload"),
            SCOPE_CONTROL
        );
        assert_eq!(
            required_scope_for(&Method::POST, "/api/sessions"),
            SCOPE_MANAGE
        );
        assert_eq!(
            required_scope_for(&Method::POST, "/api/sessions/abc/kill"),
            SCOPE_MANAGE
        );
        assert_eq!(
            required_scope_for(&Method::POST, "/api/sessions/abc/metadata"),
            SCOPE_MANAGE
        );
    }

    #[tokio::test]
    async fn api_key_verification_returns_scopes_and_caches() {
        let state = AuthState::new(hash_password("hunter2").expect("password should hash"));
        let key = "0123456789abcdef";
        let hash = hash_password(key).expect("key should hash");
        let entries = vec![(hash, "observe,node".to_string())];

        let scopes = state
            .verify_api_key_scopes(key, &entries)
            .await
            .expect("key must verify");
        assert_eq!(scopes, "observe,node");
        // Cached: second call succeeds even with an empty entry list.
        let cached = state
            .verify_api_key_scopes(key, &[])
            .await
            .expect("cached verification must succeed");
        assert_eq!(cached, "observe,node");
        // Wrong key never verifies.
        assert!(
            state
                .verify_api_key_scopes("ffffffffffffffff", &entries)
                .await
                .is_none()
        );
    }

    #[test]
    fn websocket_origin_must_match_host() {
        let mut headers = HeaderMap::new();
        // No Origin header (non-browser client) is allowed.
        assert!(ws_origin_allowed(&headers));

        headers.insert(header::HOST, "localhost:7700".parse().unwrap());
        assert!(ws_origin_allowed(&headers));

        headers.insert(header::ORIGIN, "http://localhost:7700".parse().unwrap());
        assert!(ws_origin_allowed(&headers));

        headers.insert(header::ORIGIN, "https://evil.example.com".parse().unwrap());
        assert!(!ws_origin_allowed(&headers));

        // Origin without a Host header cannot be validated: reject.
        let mut no_host = HeaderMap::new();
        no_host.insert(header::ORIGIN, "http://localhost:7700".parse().unwrap());
        assert!(!ws_origin_allowed(&no_host));
    }

    #[test]
    fn extract_request_token_prefers_authorization_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer header-token".parse().expect("header should parse"),
        );
        headers.insert(
            header::COOKIE,
            format!("{AUTH_COOKIE_NAME}=cookie-token")
                .parse()
                .expect("cookie should parse"),
        );

        let token = extract_request_token_parts(&headers, Some("token=query-token"));

        assert_eq!(token.as_deref(), Some("header-token"));
    }

    #[test]
    fn extract_request_token_reads_cookie_when_no_header_or_query() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("other=x; {AUTH_COOKIE_NAME}=cookie-token; third=y")
                .parse()
                .expect("cookie should parse"),
        );

        let token = extract_request_token_parts(&headers, None);

        assert_eq!(token.as_deref(), Some("cookie-token"));
    }

    #[test]
    fn auth_cookie_headers_include_browser_scope() {
        let set_cookie = build_auth_cookie("abc123", false);
        let secure_cookie = build_auth_cookie("abc123", true);
        let clear_cookie = clear_auth_cookie();

        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Lax"));
        assert!(!set_cookie.contains("Secure"));
        assert!(secure_cookie.contains("; Secure"));
        assert!(clear_cookie.contains("Max-Age=0"));
    }
}
