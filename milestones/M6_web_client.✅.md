# Milestone 6: Web Client

> **Status:** ✅ Done  
> **SPEC ref:** §14.3  
> **Implementation:** `web/`

## Goal

A browser-based UI for monitoring, managing, and interacting with sessions — enabling remote supervision without the CLI.

---

## Tasks

### Project scaffolding

- [x] Vite + React + TypeScript project under `web/`.
- [x] ESLint config, `tsconfig` variants.
- [x] API client module (`web/src/api/client.ts`) with typed REST + SSE + WebSocket helpers.
- [x] Shared type definitions (`web/src/api/types.ts`) mirroring server protocol types.

### Session list view (`web/src/pages/SessionsPage.tsx`)

- [x] Tabular session list with id, title, status badge, age.
- [x] Live updates via SSE (`session_created`, `session_updated`, `session_deleted`).
- [x] Search / filter / sort / pagination.
- [x] Status sparklines (`SparklineSvg.tsx`).
- [x] SSE connection status indicator (`SseStatusDot.tsx`).

### Session detail view (`web/src/pages/SessionDetailPage.tsx`)

- [x] Session metadata header (id, title, status, pid, command, cwd, timestamps).
- [x] Embedded XTerm.js terminal for log replay and live attach (`XTerm.tsx`).
- [x] WebSocket connection to `/api/sessions/:id/attach`.
- [x] Resize observer forwarding terminal dimensions to server.
- [x] Send input from browser keyboard to PTY.

### UI components

- [x] `StatusBadge` — colored status label per session state.
- [x] `SparklineSvg` — compact activity sparkline.
- [x] `SseStatusDot` — live SSE connection health indicator.
- [x] `Logo` — branding component.
- [x] `XTerm` — xterm.js wrapper with fit addon.

### Remaining / polish

- [x] Stop session action from web UI (button + confirmation).
- [x] Send ad-hoc input from web UI without full terminal attach.
- [x] Create new session form.
- [x] Mobile-responsive layout.
- [x] Dark / light theme toggle.
- [x] Error boundary and offline / reconnect UX.
- [x] End-to-end tests (Playwright or similar).
