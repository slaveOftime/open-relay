# Milestone 5: HTTP / REST + WebSocket API

> **Status:** 🔄 Mostly implemented — OpenAPI docs outstanding  
> **SPEC ref:** §14.1, §14.2, §14.4  
> **Implementation:** `src/http/`

## Goal

Expose daemon capabilities over an HTTP API so the web client and external tooling can interact without requiring the CLI binary.

---

## Tasks

### HTTP server bootstrap

- [x] Add `axum`-based HTTP server to daemon startup path.
- [x] Bind to configurable local address (default `127.0.0.1:PORT`).
- [x] CORS middleware for web client access (`tower_http::cors`).
- [x] Shared `AppState` (session store, config, DB, SSE broadcast channel).

### REST endpoints (`src/http/sessions.rs`)

- [x] `GET  /api/health` — daemon health + pid.
- [x] `GET  /api/sessions` — list sessions with filtering, pagination, sorting.
- [x] `POST /api/sessions` — create session (equivalent to `oly start`).
- [x] `GET  /api/sessions/:id` — get single session summary.
- [x] `POST /api/sessions/:id/stop` — stop session (grace period support).
- [x] `POST /api/sessions/:id/input` — send input to session.
- [x] `GET  /api/sessions/:id/logs` — read persisted logs (tail / strip-color support).

### Real-time event stream (`src/http/sse.rs`)

- [x] `GET /api/events` — SSE stream with session snapshots and lifecycle events.
- [x] Event types: `session_created`, `session_updated`, `session_deleted`, `session_notification`.
- [x] Initial snapshot burst on connect.
- [x] Broadcast channel integrating with notification dispatcher.

### WebSocket PTY streaming (`src/http/ws.rs`)

- [x] `GET /api/sessions/:id/attach` — WebSocket upgrade for interactive PTY.
- [x] Server messages: `snapshot`, `output`, `end`, `error`.
- [x] Client messages: `input`, `resize`, `detach`.
- [x] Ring-buffer snapshot sent before live stream (reattach parity with CLI).

### Push subscription management

- [x] `GET  /api/push/public-key` — return VAPID public key.
- [x] `POST /api/push/subscribe` — store push subscription (endpoint + p256dh + auth).
- [x] `DELETE /api/push/subscribe` — remove a push subscription.
- [x] DB migration `0002_create_push_subscriptions.sql`.

### OpenAPI / Swagger docs

- [ ] Add `utoipa` (or equivalent) annotations to all route handlers.
- [ ] Expose `GET /api/docs` (Swagger UI) and `GET /api/openapi.json`.
- [ ] Keep schema in sync with TypeScript types in `web/src/api/types.ts`.

---

## Milestone 5c: HTTP Password Authentication

> **Status:** ✅ Implemented

### Goal

Protect the HTTP/WebSocket daemon API with interactive password authentication so that only authorised users can reach session management endpoints, even when the port is locally accessible. Provide a safe opt-out (`--no-auth`) for deployments behind a secure gateway.

### Background

The daemon's HTTP port is bound to `127.0.0.1` by default, but any process or user on the same machine can reach it. For the remote-supervision pattern (where the port may be exposed through a tunnel), authentication is mandatory.

This milestone ships a minimal but correct first layer:
- Single shared password, set interactively at daemon start
- Argon2id password hashing (never stored plaintext)
- Short-lived in-memory Bearer tokens (cleared on daemon restart)
- Brute-force protection: 3-attempt lockout for 15 minutes
- `--no-auth` escape hatch with explicit risk acknowledgment

Full TLS + role-based access control are addressed in M9.

### Acceptance Criteria

**Done when:**
- `oly daemon start` prompts for a password (no echo); daemon refuses to start if cancelled.
- `oly daemon start --detach` prompts in the parent, hashes the password, and passes the hash to the background child via hidden `--auth-hash-internal` CLI arg.
- `GET /api/auth/status` always returns `{ "auth_required": true/false }` without a token.
- `POST /api/auth/login` with correct password returns a Bearer token; incorrect password increments a global failure counter.
- After 3 failed login attempts, `POST /api/auth/login` returns `429 Too Many Requests` with `Retry-After` header for 15 minutes.
- All `/api/*` endpoints (except `/api/health` and `/api/auth/*`) return `401 Unauthorized` when no valid token is provided.
- `POST /api/auth/logout` removes the token from the active set.
- `oly daemon start --no-auth` prints a risk warning and requires the user to type `yes` before starting.
- No-auth mode: all `/api/*` endpoints are accessible without a token.
- Web UI: a full-screen non-dismissible login dialog appears when `auth_required: true` and no valid token is present.
- Web UI: wrong password shows remaining attempt count; after lockout shows a live countdown.
- Web UI: "Sign out" button in the top-right header clears the token and returns to the login dialog.

### Tasks

#### Backend

- [x] Add `argon2` and `rpassword` crate dependencies (`Cargo.toml`).
- [x] Extend `DaemonStartArgs` with `--no-auth` and hidden `--auth-hash-internal` flags (`src/cli.rs`).
- [x] Implement `AuthState` with Argon2id verification, in-memory token map, and global lockout state (`src/http/auth.rs`).
- [x] Implement `GET /api/auth/status`, `POST /api/auth/login`, `POST /api/auth/logout` handlers (`src/http/auth.rs`).
- [x] Add `auth: Option<Arc<AuthState>>` to `AppState`; wire `require_auth` middleware before all `/api/*` routes (`src/http/mod.rs`).
- [x] Add `prompt_and_hash_password()` helper using `rpassword`; add `confirm_no_auth_risk()` for `--no-auth` acknowledgment (`src/daemon.rs`).
- [x] Update `daemon::start()` to accept `no_auth: bool` and `auth_hash_internal: Option<String>`; propagate hash to `spawn_detached()` and `run_foreground()` (`src/daemon.rs`).
- [x] Pass `no_auth` and `auth_hash_internal` from `DaemonStartArgs` to `daemon::start()` (`src/main.rs`).

#### Frontend

- [x] Add `AuthStatus`, `LoginResponse`, `AuthRequiredError`, `TooManyAttemptsError` types (`web/src/api/types.ts`).
- [x] Add token helpers (`getToken`, `setToken`, `clearToken`), auth functions (`getAuthStatus`, `login`, `logout`), Bearer header injection, and 401 interceptor dispatching `oly:auth-required` event (`web/src/api/client.ts`).
- [x] Implement `LoginDialog` component: non-dismissible overlay, password input, attempt-count display, lockout countdown (`web/src/components/LoginDialog.tsx`).
- [x] Add auth state + `LoginDialog` + logout button to `App.tsx` (`web/src/App.tsx`).

### Security Notes

- Password is **never stored on disk**; hash lives in process memory only and is cleared on daemon restart.
- Argon2id with a random `OsRng` salt is used — resistant to GPU and rainbow-table attacks.
- The PHC hash (not the plaintext password) is passed via CLI arg to the detached child. The hash alone cannot be used to authenticate; it can only verify the original password.
- Lockout is **global** (not per-IP) — appropriate for a single-user local tool.
- `sessionStorage` is used for the browser token — automatically cleared when the browser tab closes.
- `/api/health` and `/api/auth/*` deliberately bypass auth for liveness probes and the login flow itself.
- IPC (CLI commands) is a separate trust boundary protected by OS peer-credential validation (M4).
