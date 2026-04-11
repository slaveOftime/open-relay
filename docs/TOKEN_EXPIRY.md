# Expiring Session Tokens in `oly`

**Status:** implemented
**Date:** 2026-04-10
**Related audit:** [SECURITY_AUDIT_REPORT.md](./SECURITY_AUDIT_REPORT.md) — finding M-1

---

## Problem

Before this change, session tokens issued by `oly`'s HTTP authentication system had **no expiration**. A token stored in a browser cookie or captured from an access log remained valid for the entire lifetime of the daemon process — potentially weeks or months. If a token leaked, there was no way to revoke it short of restarting the daemon and invalidating every active session at once.

The security audit flagged this as **M-1: Session Token Never Expires**.

---

## What changed

Session tokens now carry a **creation timestamp** and are validated against a **configurable maximum age**. Tokens older than the configured TTL are rejected at the authentication middleware layer, forcing a fresh login.

### Implementation details

1. **Token storage migration** — the in-memory `HashSet<String>` was replaced with a `HashMap<String, TokenEntry>` where each entry tracks `issued_at: SystemTime`.

2. **Default TTL** — tokens expire after **24 hours** by default. This can be overridden via `config.json` under the `auth` section:

   ```json
   {
     "auth": {
       "token_ttl_hours": 48
     }
   }
   ```

3. **Lazy cleanup** — expired entries are purged during the authentication check of the next request, keeping the in-memory set bounded without a separate garbage-collection thread.

4. **Graceful degradation** — if the system clock shifts backward, the token is treated as still valid until forward time catches up. No false invalidations.

---

## Why this matters

- A leaked token from a browser history entry, proxy log, or `Referer` header stops being useful after the TTL window.
- Long-running daemons no longer accumulate unlimited token entries from users who log in repeatedly.
- The change is fully backward-compatible: existing tokens issued before the upgrade will still work until the daemon restarts or the TTL elapses naturally.

---

## What's next

The token expiry work is one item from the broader security audit follow-up. Other findings being addressed include:

- Per-IP login lockouts (already shipped)
- `Secure` cookie flag behind TLS proxies (already shipped)
- Bounded IPC line reads (already shipped)
- Stricter trust around `X-Forwarded-For` headers (already shipped)
- CORS hardening (in progress)

See [SECURITY_AUDIT_REPORT.md](./SECURITY_AUDIT_REPORT.md) for the full audit findings and priority order.
