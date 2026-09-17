# ADR-0007: Auth scopes, browser sessions, proxy isolation, side effects

- Status: Accepted (direction; expiration durations and admin UX deferred to M5)
- Plan reference: PLAN.md §10.3 (invariant I12)

## Context

Browser auth is one deterministic token derived from the password hash: no
expiry, no revocation short of changing the password, stable across
restarts. There are no scoped credentials — anything authenticated can do
everything. Untrusted recordings can carry clipboard/hyperlink side effects
into any renderer that replays them.

## Decision (draft)

1. Random, revocable, expiring browser sessions stored server-side.
   Logout/revocation invalidates open control streams, not just future
   requests.
2. Scoped machine credentials: `observe`, `input/control`, `manage`,
   `node`, enforced server-side per session/node. A frontend read-only flag
   is not access control.
3. WebSocket Origin validation, cookie-mutation CSRF policy, secure cookies,
   trusted-proxy rules, and TLS deployment requirements are specified and
   tested. No reusable query-string credentials.
4. Reverse-proxied apps are credential-isolated: oly cookies/Authorization
   are never forwarded to arbitrary upstreams as implicit SSO.
5. Replay is side-effect-free: no child stdin writes, clipboard sets, URL
   launches, uploads, or hook execution. Recordings are private by default;
   input content is not recorded by default.

## Rejected alternatives

- Keeping deterministic password-derived tokens (leak = permanent access;
  rotation logs out every browser).
- Perimeter-only authorization (breaks under federation and proxies).

## Acceptance

- Re-audit against current code (the 2025 audit predates existing
  permissions/limits work); publish verified fixes and remaining risks.
- Revoked-controller write attempts fail on live streams.
- Fuzz/pen tests: traversal, decompression bombs, frame limits, replay side
  effects, node compromise boundaries.

## Migration

1.0 upgrade requires re-login and credential reissue; insecure sessions are
not carried over for convenience.
