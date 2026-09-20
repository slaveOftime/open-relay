# ADR-0007: Auth scopes, browser sessions, proxy isolation, side effects

- Status: Accepted; M5-4 implemented items 1–4 (session registry, scopes, origin/CSRF posture, proxy isolation)
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

0.5.0 upgrade requires re-login and credential reissue; insecure sessions are
not carried over for convenience.

## Addendum (M5-4): implemented authz

1. **Random, revocable, expiring sessions**: login issues a 256-bit random
   token stored server-side (7-day TTL; cookie mirrors it). Logout revokes
   the token and bumps a revocation epoch; open WebSocket attach streams
   (local and proxied) and SSE event streams select on the epoch, re-validate
   their token, and close loudly on revocation. A daemon restart invalidates
   all sessions (in-memory registry; re-login required). The deterministic
   password-derived token is gone — and with it the cross-instance token
   sharing that let one cookie authenticate to every upstream oly behind the
   reverse proxy.
2. **Scoped machine credentials**: API keys carry a comma-separated scope
   list (`observe`, `control`, `manage`, `node`, `all`; migration 0007).
   `oly api-key add NAME --scopes ...` (default `node`, preserving the
   historical node-join use). Node join requires the `node` scope. Presented
   as `Authorization: Bearer` on the HTTP API, a key authorizes routes
   classified by capability: GETs are `observe` (attach upgrades are
   `control`), `/input` and `/upload` are `control`, all other mutations are
   `manage`. Verified key→scope mappings are cached for 60 s to keep Argon2
   verification off the hot path. Query-string and cookie credentials are
   never treated as API keys.
3. **Origin/CSRF posture**: WebSocket attach upgrades reject an `Origin`
   whose host does not match the `Host` header or a gateway-provided
   `X-Forwarded-Host` (non-browser clients without `Origin` are unaffected).
   `X-Forwarded-Host` is safe to trust here because the check only defends
   against browsers, and the browser WebSocket API cannot set arbitrary
   handshake headers — a non-browser client able to spoof it could simply
   omit `Origin`. This keeps attach working behind HTTPS gateways that
   rewrite `Host` to the upstream address (nginx's default without
   `proxy_set_header Host $host`). The session cookie is `HttpOnly; SameSite=Lax`
   (`Secure` when TLS is detected), so cross-site POSTs cannot ride it.
   Query-string tokens remain accepted only on the WS/SSE upgrade paths where
   browsers cannot set headers; with random, expiring, revocable tokens the
   leakage window is now bounded (accepted residual risk).
4. **Proxy credential isolation**: the reverse proxy strips `Authorization`
   and `Cookie` from requests and WebSocket handshakes forwarded to upstream
   apps — oly credentials are never forwarded as implicit SSO. Upstream oly
   instances behind the proxy now require their own login.

Deferred: admin UX for session listing/revocation beyond logout; per-scope
frontend affordances (a read-only UI flag is not access control, but the UI
does not yet hide disallowed actions); item 5 (replay side-effect policy) is
audited in M5-6.
