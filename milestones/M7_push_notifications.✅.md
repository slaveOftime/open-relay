# Milestone 7: PWA Push Notifications

> **Status:** ✅ Done  
> **SPEC ref:** §14.4  
> **Implementation:** `migrations/0002_create_push_subscriptions.sql`, `src/http/sessions.rs`, `src/db.rs`, `src/notification/channel.rs`, `src/daemon.rs`, `web/src/lib/push.ts`, `web/src/pages/SessionsPage.tsx`, `web/src/main.tsx`, `web/public/sw-push.js`, `web/public/manifest.webmanifest`, `web/public/offline.html`, `web/vite.config.ts`, `web/dev-dist/`

## Goal

Enable the web client to receive push notifications from the daemon even when the browser tab is not focused, supporting the async supervision model from anywhere with an open browser session.

---

## Tasks

### Service worker (browser-side)

- [x] Push event handler in `web/public/sw-push.js` (shows OS notification from push payload).
- [x] Notification click handler (focuses / opens the app window).
- [x] Workbox pre-cache integration in `web/dev-dist/sw.js`.

### Push subscription management (server-side)

- [x] DB migration: `push_subscriptions` table.
- [x] `Database::insert_push_subscription` / `delete_push_subscription` / `list_push_subscriptions`.
- [x] REST endpoints: `GET /api/push/public-key`, `POST /api/push/subscribe`, `DELETE /api/push/subscribe`.
- [x] VAPID public key exposed via config (`AppConfig::web_push_vapid_public_key`).

### Web client subscription flow

- [x] Browser permission prompt + `pushManager.subscribe()` call.
- [x] POST subscription to `/api/push/subscribe` on permission grant.
- [x] Remove subscription on permission revoke or explicit unsubscribe.
- [x] UI indicator showing push notification opt-in status.

### Daemon-side push dispatch

- [x] Load subscriptions from DB when notification event fires.
- [x] Encrypt and send Web Push message (RFC 8291) using VAPID private key.
- [x] Payload: session id, notification kind, summary excerpt (matches local OS notification payload).
- [x] Handle expired / invalid subscriptions (remove from DB on `410 Gone`).
- [x] Configurable: disable push dispatch if no VAPID private key is set.

### PWA manifest + install

- [x] `web/public/manifest.webmanifest` with icons, name, display, theme color.
- [x] Service worker registration in `web/src/main.tsx` (Vite PWA plugin or manual).
- [x] Offline fallback page (basic "daemon unreachable" shell).

### Verification

- [x] Push notification arrives in browser when session triggers the input-needed detector, with browser tab closed.
- [x] Notification click navigates to the correct session detail view.
