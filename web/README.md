# Open Relay web client

React + TypeScript single-page app for the Open Relay daemon: session list,
terminal attach, and the mobile-friendly toolbars around them.

## Running it

```bash
npm run dev          # dev server on http://127.0.0.1:8060 (proxies /api to :15443)
npm run build        # type-check + production build
npm run preview      # serve the production build
```

The daemon must be running for anything useful to happen; `npm run dev` only
proxies to it.

## Checks

| Command                | What it runs                                                                    |
| ---------------------- | ------------------------------------------------------------------------------- |
| `npm run typecheck`    | `tsc -b` (strict: `noUnusedLocals`, `noUnusedParameters`, `erasableSyntaxOnly`) |
| `npm run lint`         | ESLint 9 flat config over `src` + `e2e`                                         |
| `npm test`             | Vitest unit tests, colocated (`*.test.ts`)                                      |
| `npm run format:check` | Prettier over the whole repo                                                    |
| `npm run test:e2e`     | Playwright specs in `e2e/` (Chromium)                                           |

CI (`.github/workflows/ci-web.yml`) runs all of them. Run `npm run format:check`
after editing — Prettier and ESLint must both agree.

## Layout

```
src/
├── api/          transport: REST, attach WebSocket, SSE, shared types
├── components/
│   ├── ui/         Radix-backed primitives (Button, Dialog, Badge, ...)
│   ├── attach/     the attach drawer and its storage / history / image cache
│   ├── dialogs/    every dialog in the app, plus their shared helpers
│   ├── sessions/   session row, card, tags, pin, notification toggle, skeletons
│   ├── sparkline/  the activity sparkline and its geometry / metrics / model
│   └── terminal/   xterm wrapper, quick keys, hold-to-repeat, scroll wheel
├── hooks/         shared React hooks (repeat-while-pressed, session events)
├── lib/           domain modules and stateful services
├── pages/         route composition (SessionsPage, SessionDetailPage)
└── utils/         pure helpers (formatting, key parsing, ANSI, ordering, colors)
```

Rules of thumb, and the longer version in `FRONTEND.md`:

- Pages compose; `components/` own behavior; `components/ui/` owns primitives.
  Never put a page-local design system in `pages/`.
- New primitives only when a pattern repeats, and only in `components/ui/`.
- Components are `PascalCase.tsx`; every other module is `kebab-case.ts`.
- Pure logic lives next to its component (`attach-panel-input.ts`,
  `sparkline-geometry.ts`, `quick-keys.ts`) so it can be unit-tested without a DOM.
- `utils/` and `lib/` never import from `components/` or `pages/`.

## Related docs

| File                | Contents                                                      |
| ------------------- | ------------------------------------------------------------- |
| `FRONTEND.md`       | layering, conventions, and how to use the design system       |
| `DESIGN.md`         | the visual system: tokens, density, color rules               |
| `REFACTOR-PLAN.md`  | structure/size plan and how the frontend is being reorganized |
| `ISSUES-BACKLOG.md` | findings recorded for later, not fixed yet                    |
