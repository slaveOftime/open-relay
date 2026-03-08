use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::http::AppState;

const MAX_FAILED_ATTEMPTS: u32 = 3;
const LOCKOUT_DURATION: Duration = Duration::from_secs(15 * 60); // 15 minutes
/// How often the background task sweeps the lockout table for expired entries.
const LOCKOUT_CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60); // 5 minutes

// ── AuthState ────────────────────────────────────────────────────────────────

pub struct AuthState {
    password_hash: String,
    tokens: Mutex<HashSet<String>>,
    /// Per-IP lockout table. Each client is tracked independently so that a
    /// brute-force attempt from one IP cannot lock out legitimate users.
    lockout: Mutex<HashMap<IpAddr, LockoutRecord>>,
}

#[derive(Default)]
struct LockoutRecord {
    failed_attempts: u32,
    locked_until: Option<Instant>,
}

enum FailureOutcome {
    LockedOut { until: Instant },
    AttemptsRemaining(u32),
}

impl AuthState {
    pub fn new(password_hash: String) -> Arc<Self> {
        let state = Arc::new(Self {
            password_hash,
            tokens: Mutex::new(HashSet::new()),
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
                lockout.retain(|_, r| r.locked_until.map_or(true, |t| now < t));
                let removed = before - lockout.len();
                if removed > 0 {
                    debug!(
                        removed,
                        "auth: background cleanup evicted expired lockout records"
                    );
                }
            }
        });
    }

    pub async fn is_valid_token(&self, token: &str) -> bool {
        self.tokens.lock().await.contains(token)
    }

    /// Returns `Some(locked_until)` if the IP is currently locked out.
    /// Evicts the record and returns `None` if the lockout has expired.
    async fn locked_until(&self, ip: IpAddr) -> Option<Instant> {
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
    async fn record_failure(&self, ip: IpAddr) -> FailureOutcome {
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
/// Checks `X-Real-IP` first (nginx single-proxy style), then the first entry
/// of `X-Forwarded-For`, then falls back to the direct peer IP. Allows correct
/// per-client lockout when the daemon is exposed through a reverse-proxy tunnel.
fn effective_ip(headers: &HeaderMap, peer: IpAddr) -> IpAddr {
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
        return (StatusCode::OK, Json(serde_json::json!({ "token": "" }))).into_response();
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
        let token = uuid::Uuid::new_v4().to_string();
        auth.tokens.lock().await.insert(token.clone());
        info!(ip = %client_ip, "auth: login success — token issued");
        return (StatusCode::OK, Json(LoginResponse { token })).into_response();
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

/// POST /api/auth/logout — revoke the caller's token.
pub async fn logout(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let client_ip = effective_ip(&headers, peer.ip());

    let Some(auth) = &state.auth else {
        return StatusCode::OK.into_response();
    };

    if let Some(token) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        let removed = auth.tokens.lock().await.remove(token);
        if removed {
            info!(ip = %client_ip, "auth: logout — token revoked");
        } else {
            debug!(ip = %client_ip, "auth: logout — token not found (already expired?)");
        }
    }

    StatusCode::OK.into_response()
}

// ── Auth Middleware ──────────────────────────────────────────────────────────

/// Axum middleware: enforce Bearer-token authentication for all `/api/` routes
/// except `/api/health` and `/api/auth/*`.
pub async fn require_auth(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Some(auth) = state.auth.as_ref().map(Arc::clone) else {
        // No-auth mode: pass through unconditionally.
        return next.run(request).await;
    };

    let path = request.uri().path().to_owned();

    // Always public: health probe, auth endpoints, and non-API (static assets).
    if path == "/api/health" || path.starts_with("/api/auth/") || !path.starts_with("/api/") {
        return next.run(request).await;
    }

    // Extract Bearer token from the Authorization header, or fall back to the
    // `?token=` query parameter (required for SSE/WebSocket where browsers
    // cannot set custom headers via EventSource / WebSocket APIs).
    let token = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_owned())
        .or_else(|| {
            request.uri().query().and_then(|q| {
                q.split('&')
                    .find(|p| p.starts_with("token="))
                    .and_then(|p| p.strip_prefix("token="))
                    .map(|t| t.to_owned())
            })
        });

    if let Some(ref token) = token {
        if auth.is_valid_token(token).await {
            debug!(path = %path, "auth: authorized request");
            return next.run(request).await;
        }
    }

    // Unauthorized — log with client IP (ConnectInfo is in request extensions
    // because the server is started with into_make_service_with_connect_info).
    let client_ip = {
        let peer_ip = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip());
        peer_ip
            .map(|ip| effective_ip(request.headers(), ip).to_string())
            .unwrap_or_else(|| "unknown".to_string())
    };

    warn!(ip = %client_ip, path = %path, "auth: unauthorized request rejected");

    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "unauthorized" })),
    )
        .into_response()
}
