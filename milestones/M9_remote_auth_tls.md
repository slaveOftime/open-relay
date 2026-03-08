# Milestone 9: Built-in Remote Auth and TLS Boundary

> **Status:** ⏳ Not started  
> **SPEC ref:** §14.6, §15

## Goal

Allow operators to expose the daemon to remote clients securely without requiring an external tunnel or auth gateway — while preserving the option to use the external-gateway pattern from §15.

---

## Background

The MVP remote-supervision pattern (§15) places authN/authZ in an operator-managed external gateway. This milestone builds a first-class alternative where the daemon owns the TLS + auth boundary directly.

---

## Tasks

### TLS listener

- [ ] Add optional TLS mode to the HTTP server (bind to `0.0.0.0` or configurable address).
- [ ] Support PEM certificate + private key from config or auto-generated self-signed cert.
- [ ] Let's Encrypt ACME integration (optional, gated by config flag).

### Authentication

- [ ] Bearer token authentication for HTTP/WebSocket endpoints (at minimum: static shared secret from config).
- [ ] Optional: short-lived token issuance (TOTP / HMAC time-window) for operator-generated access codes.
- [ ] Reject unauthenticated requests with `401 Unauthorized` before any session data is returned.

### Authorization

- [ ] Role model: `viewer` (read-only: list, logs, SSE) vs `operator` (all including start/stop/input).
- [ ] API key scopes stored in config.

### Audit trail for remote actions

- [ ] Remote caller identity (token id / IP) attributed in `events.log` for every request.

### Config additions

- [ ] `http.tls_cert`, `http.tls_key` paths.
- [ ] `http.auth.tokens`: list of `{ id, secret, role }`.
- [ ] `http.bind`: override listen address (default stays `127.0.0.1`).

### Verification

- [ ] Remote `curl` with valid bearer token succeeds; without token returns 401.
- [ ] Read-only token cannot stop or send input to sessions.
- [ ] TLS connection validated against self-signed cert with pinned fingerprint.
