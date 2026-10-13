# Open Relay web client

React + TypeScript UI for the oly daemon: session listing, browser terminal
attach, and mobile-friendly supervision controls.

## Run

```sh
npm ci
npm run dev          # http://127.0.0.1:8060; /api proxies to daemon :15443
npm run build        # TypeScript check + production bundle
npm run preview      # serve the production bundle
```

A daemon is required for real sessions. The release binary embeds `web/dist`;
debug builds use the development frontend. Use isolated daemon state when
working on the project; see [Contributing](../CONTRIBUTING.md).

## Checks

| Command                                  | What it runs                                         |
| ---------------------------------------- | ---------------------------------------------------- |
| `npm run build`                          | `tsc -b` plus Vite production build                  |
| `npm run lint`                           | ESLint flat config                                   |
| `npm test`                               | Vitest colocated `*.test.ts` / `*.test.tsx`          |
| `npm run format:check`                   | Prettier                                             |
| `npm run e2e -- --project=chromium`      | Playwright Chromium specs                            |
| `npm run e2e -- --project=mobile-safari` | Opt-in WebKit mobile specs; browser install required |

Install the matching Playwright browser before E2E (`npx playwright install
chromium`, or `webkit`). The E2E server builds and previews the UI on port 4173;
specs mock API transports and do not certify a shipped daemon's auth/PTY behavior.
CI runs Chromium; mobile Safari and actual phone/PWA behavior need separate evidence.

## Layout

```text
src/
├── api/          REST, attach WebSocket, SSE, shared types
├── components/
│   ├── ui/         Radix-backed primitives
│   ├── attach/     attach drawer, storage, history, image cache
│   ├── dialogs/    session and metadata dialogs
│   ├── sessions/   rows, cards, tags, pins, status, skeletons
│   ├── sparkline/  activity geometry and model
│   └── terminal/   xterm, quick keys, fonts, theme, input
├── hooks/        reconnect, gestures, viewport and keyboard behavior
├── lib/          domain modules and stateful services
├── pages/        route composition
└── utils/        pure formatting, parsing, ordering helpers
```

Follow [Frontend conventions](./FRONTEND.md): pages compose; components own
behavior; UI primitives stay in `components/ui/`. Components use PascalCase;
other modules use kebab-case. Keep pure logic testable without a DOM and avoid
`utils`/`lib` imports from the view layer.

## Boundaries and related docs

Custom apps/proxies are separate from this UI and require opt-in trusted code;
see [Apps skill](../skills/oly-apps/SKILL.md). Never relax the control-plane CSP
just to run arbitrary generated reports.

- [Design](./DESIGN.md) — visual tokens, density, colors
- [Refactor plan](./REFACTOR-PLAN.md) — structural work
- [Backlog](./ISSUES-BACKLOG.md) — recorded findings, not release certification
- [Compatibility](../docs/COMPATIBILITY.md) — beta contract
