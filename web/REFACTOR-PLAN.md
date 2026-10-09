# Web refactor plan

Status: **proposal** — no code has been changed by this document.
Goal: make `web/src` consistent, easy to navigate, and cheap to maintain
**without changing behavior, UX, or the public component APIs**.

Ground rules first, because they are what makes this safe:

1. **Move-only refactors.** Each step moves code verbatim (use `git mv` so history
   follows). No logic edits, no renames of props/state, no "while I'm here" fixes.
2. **Behavior changes are out of scope.** Anything that looks like a bug or a UX
   improvement goes into `web/ISSUES-BACKLOG.md` and is fixed separately.
3. **Gates stay green at every commit.** `npx tsc -b`, `npx eslint src e2e`,
   `npx vitest run` (243 tests / 30 files), `npx playwright test` (14 tests / 6
   specs) — the same four gates CI runs.
4. **Small commits, one concern each.** Reviewers should be able to read a diff and
   see only moved lines (`git diff -M`).

Guides this plan follows (read before starting a phase):

- [Radix UI — Composition](https://www.radix-ui.com/primitives/docs/guides/composition):
  leaf components spread props and forward refs; compose with `asChild` rather than
  re-implementing.
- [react.dev — Thinking in React](https://react.dev/learn/thinking-in-react) and
  [Reusing Logic with Custom Hooks](https://react.dev/learn/reusing-logic-with-custom-hooks):
  small single-responsibility components, logic extracted into hooks.
- [React Folder Structure Best Practices (Robin Wieruch)](https://www.robinwieruch.de/react-folder-structure/):
  group by feature/domain, keep the grouping consistent, don't over-nest.
- shadcn/ui guidance ("components are source code, not a dependency"): primitives
  live in the repo and are edited like app code — already true here.
- **Local source of truth wins on conflicts:** `web/FRONTEND.md` (layering and
  conventions) and `web/DESIGN.md` (visual system). This plan aligns with them and
  does not re-litigate them.

---

## 1. Where we are today

Measured, not estimated (110 text files in `web/src`):

| Metric                          | Value                                                                                                 |
| ------------------------------- | ----------------------------------------------------------------------------------------------------- |
| Total lines in `src`            | 19,641                                                                                                |
| Test files / test LOC           | 32 files / 3,507 LOC                                                                                  |
| Largest file                    | `pages/SessionsPage.tsx` — 2,514 lines                                                                |
| 2nd / 3rd largest               | `pages/SessionDetailPage.tsx` — 1,973 · `components/AttachPanel.tsx` — 862                            |
| Also large                      | `components/XTerm.tsx` — 721 · `api/client.ts` — 694 · `components/SparklineSvg.tsx` — 603            |
| UI primitives (`components/ui`) | 23 files / 1,380 lines (3 unused — see backlog)                                                       |
| Stack                           | React 19.2.8, TS 6.0.3, Vite 8.1.5, Tailwind 4.3.3, Vitest 4.1.10, Playwright 1.63, 13 Radix packages |

The architecture is already sound and mostly matches `FRONTEND.md`. The problems
are **size** (three files carry ~27% of the codebase), **naming drift**
(`lib/utils` vs `utils/*`, extensions in some import specifiers), **homes for
hooks** (none — hooks live next to the one component that uses them), and a
**couple of layer inversions** (`lib/` importing from `components/`).

---

## 2. Target structure

```
src/
├── api/                 transport only: REST, attach WebSocket, SSE
├── components/
│   ├── ui/              Radix-backed primitives (unchanged location)
│   ├── attach/          AttachPanel + its extracted modules + mobile key bar
│   ├── sessions/        session list pieces (row, card, tags, pin, skeletons…)
│   ├── dialogs/         every dialog in the app
│   ├── terminal/        xterm wrapper + quick keys + repeat/scroll primitives
│   └── sparkline/       sparkline component + its geometry/metrics/store
├── hooks/               every shared React hook (new)
├── lib/                 domain + stateful modules (sessions, push, storage, history)
├── utils/               pure framework-agnostic helpers
└── pages/               route composition + page-local pure modules
```

### Naming rules

| Rule                                                                               | Example                                                                     |
| ---------------------------------------------------------------------------------- | --------------------------------------------------------------------------- |
| A file whose default export is a component is `PascalCase.tsx`                     | `SessionsPage.tsx`, `SessionRow.tsx`, `Button.tsx`                          |
| Everything else is `kebab-case.ts`                                                 | `sessions-page-prefs.ts`, `keyInput.ts`, `historyController.ts`             |
| Tests are colocated and never separate by type                                     | `quick-keys.test.ts` next to `quick-keys.ts`                                |
| No file extensions in import specifiers                                            | `@/api/types`, never `@/api/types.ts`                                       |
| Alias `@/…` when crossing a folder boundary; relative only for siblings            | `./quick-keys` inside `components/terminal/`, `@/lib/utils` everywhere else |
| One component per file; file-local sub-components only if <60 lines and not reused | —                                                                           |

### Size budget (guideline, enforced softly)

| Kind           | Target     | Hard warning                   |
| -------------- | ---------- | ------------------------------ |
| Page component | ≤400 lines | `eslint max-lines` warn at 500 |
| App component  | ≤300 lines | warn at 500                    |
| Hook / module  | ≤250 lines | warn at 400                    |

### Layer rules (enforced with `eslint-plugin-import` `no-restricted-paths` or `boundaries`)

```
utils/      → nothing internal (pure)
lib/        → api, utils
hooks/      → api, lib, utils            (no rendering)
components/ → ui, hooks, lib, utils, api
pages/      → components, hooks, lib, utils, api
```

This is the rule that kills the two current inversions:
`lib/quickKeysStorage.ts → components/terminal/quick-keys` and
`lib/sessionActivity.ts → components/sparklineStore`.

---

## 3. File splits (the actual work)

Each entry: current file → new modules. "Logic" lines are non-JSX; "JSX" lines are
markup. Moves are verbatim; nothing is rewritten.

### 3.1 `pages/SessionsPage.tsx` (2,514)

| New module                                          | From lines                            | Why it can move today                                                                                                           |
| --------------------------------------------------- | ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `pages/sessions-page-prefs.ts`                      | 180–253                               | prefs + column settings load/save, defaults                                                                                     |
| `pages/sessions-page-data.ts`                       | 273–311, 1146–1302                    | `sessionPageRequests`, request key, `fetchSessionsOnce`, `loadLocal`, `loadRemote`, `reloadSessions` (takes injected callbacks) |
| `pages/sessions-page-tags.ts`                       | 299–311                               | `normalizeSessionTags` (pure)                                                                                                   |
| `hooks/use-session-list-gestures.ts`                | 1554–1686                             | mobile pull/refresh + swipe drawing (rAF + DOM)                                                                                 |
| `hooks/use-session-table-columns.ts`                | 1804–1871                             | column resize/reorder handlers                                                                                                  |
| `components/sessions/SessionRow.tsx`                | 479–753                               | already memoized, already prop-driven                                                                                           |
| `components/sessions/SessionCard.tsx`               | 757–970                               | mobile card variant                                                                                                             |
| `components/sessions/SessionTagList.tsx`            | 319–330                               | —                                                                                                                               |
| `components/sessions/SessionNotificationButton.tsx` | 332–360                               | —                                                                                                                               |
| `components/sessions/SessionPinButton.tsx`          | 362–400                               | —                                                                                                                               |
| `components/sessions/SessionSkeletons.tsx`          | 402–430                               | row + card skeletons                                                                                                            |
| `components/sessions/GroupHeaderLabel.tsx`          | 432–450                               | —                                                                                                                               |
| `components/sessions/SortIcon.tsx`                  | 972–990                               | —                                                                                                                               |
| `components/sessions/SessionsEmptyState.tsx`        | 992–1012                              | —                                                                                                                               |
| `pages/SessionsPage.tsx`                            | keeps 1016–1550, 1688–1800, 1873–2510 | composition + handlers + render                                                                                                 |

Result: the page drops to roughly 700–900 lines (composition + handlers + render),
and every extracted piece gets its own test file next to it.

### 3.2 `pages/SessionDetailPage.tsx` (1,973)

| New module                          | From lines           | Why                                                                               |
| ----------------------------------- | -------------------- | --------------------------------------------------------------------------------- |
| `hooks/use-attach-socket.ts`        | 727–1038             | the WebSocket effect, reconnect policy, watchdog (one cohesive ~310-line concern) |
| `pages/session-detail-output.ts`    | 85–125, 285–354      | snapshot normalization + enqueue/flush pipeline (pure + refs)                     |
| `pages/session-detail-replay.ts`    | 1183–1343            | replay `step()` and byte-budget math (pure, unit-testable)                        |
| `pages/session-detail-idle.ts`      | 223–279              | idle-border animation state machine                                               |
| `pages/session-detail-reconnect.ts` | 361–386              | trace + backoff policy (pure)                                                     |
| `pages/session-detail-logs.ts`      | 1040–1128            | logs/tail/replay loading effect                                                   |
| `pages/SessionDetailPage.tsx`       | keeps state + render | composition                                                                       |

### 3.3 `components/AttachPanel.tsx` (862)

| New module                                    | From lines     | Why                                                                                     |
| --------------------------------------------- | -------------- | --------------------------------------------------------------------------------------- |
| `components/attach/attach-panel-storage.ts`   | 36–153         | 11 `localStorage`/`sessionStorage` helpers (already the file's own pattern)             |
| `components/attach/attach-input-history.ts`   | 30–34, 36–66   | history load/save + frequency ranking                                                   |
| `components/attach/attach-panel-send.ts`      | 389–430        | `handleCustomSend`/`handleSendKeySpec`/`handleSendCustomKeys` (pure-ish, take a sender) |
| `components/attach/attach-mobile-key-bar.tsx` | 766–852        | the `sm:hidden` arrow/tab/esc/enter bar                                                 |
| `components/AttachPanel.tsx`                  | keeps the rest | composition + drawer state                                                              |

### 3.4 `components/XTerm.tsx` (721)

| New module                            | From lines                              | Why                                                |
| ------------------------------------- | --------------------------------------- | -------------------------------------------------- |
| `components/terminal/xterm-theme.ts`  | 76–126                                  | `getTerminalTheme` (pure data)                     |
| `components/terminal/xterm-fonts.ts`  | 15–74                                   | font family/preload constants + loader             |
| `utils/scroll-container.ts`           | 268–285 (AttachPanel) + 382–399 (XTerm) | **dedupe** the two copies of `findScrollContainer` |
| `hooks/use-terminal-keyboard-sync.ts` | 401–441                                 | iOS soft-keyboard scroll sync                      |
| `components/XTerm.tsx`                | keeps terminal lifecycle + render       | —                                                  |

### 3.5 `components/SparklineSvg.tsx` (603)

| New module                                   | From lines   | Why                                                                                      |
| -------------------------------------------- | ------------ | ---------------------------------------------------------------------------------------- |
| `components/sparkline/sparkline-geometry.ts` | 514–603      | pure path/scale math — **already exists** as `sparklineGeometry.ts`, so this is a dedupe |
| `hooks/use-sparkline-animation.ts`           | 149–315      | the rAF draw loop + frame budget                                                         |
| `components/sparkline/sparkline-palettes.ts` | 39–63        | palettes + tooltip label                                                                 |
| `components/SparklineSvg.tsx`                | keeps render | —                                                                                        |

### 3.6 `api/client.ts` (694)

| New module                    | Contents                                                                       |
| ----------------------------- | ------------------------------------------------------------------------------ |
| `api/client-rest.ts`          | session CRUD, nodes, push                                                      |
| `api/client-attach-socket.ts` | `AttachSocket` (binary frames)                                                 |
| `api/client-sse.ts`           | the SSE event source + session event store bridge                              |
| `api/client-auth.ts`          | token storage/`getToken`                                                       |
| `api/client.ts`               | re-exports the public surface so the ~18 import sites stay unchanged in step 1 |

### 3.7 Folder regrouping (last phase, optional)

Once components are split, move them into `attach/`, `sessions/`, `dialogs/`,
`terminal/`, `sparkline/` (see §2). Every move is a `git mv` — zero content edits.
Do this **after** the splits so each move is small and the diffs stay readable.

---

## 4. Phases

Ordered by risk: config/hygiene first, structural moves next, deep splits last.
Each phase ends with all four gates green and one or more commits.

| Phase | Scope                                                                                                                                                                                   | Risk   | Gate                  |
| ----- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------ | --------------------- |
| 0     | Guardrails: fix vitest `include` (`.tsx` tests), ignore `dev-dist` in eslint, delete 3 unused `ui/` primitives, add `max-lines` warning, replace template `web/README.md`               | none   | 4 gates               |
| 1     | Naming/import consistency: drop extensions in `NodeSelector.tsx`, pick alias-vs-relative rule, remove commented-out blocks, remove `export { Badge }` from `SessionsPage.tsx`           | none   | 4 gates               |
| 2     | `utils/` for pure helpers (`cn`, `scroll-container`), `hooks/` for the 3 existing hooks, split `lib/sessionEvents.ts` (pure router vs hooks), fix the two `lib → components` inversions | low    | 4 gates               |
| 3     | Split `api/client.ts` behind re-exports                                                                                                                                                 | low    | 4 gates               |
| 4     | Pure-logic extraction: `SparklineSvg`, `XTerm`, `AttachPanel` storage, `SessionDetailPage` pure helpers — **characterization tests first** for anything without them                    | medium | 4 gates + diff review |
| 5     | Page decomposition: `SessionsPage` sub-components, `SessionDetailPage` socket/replay effects                                                                                            | medium | 4 gates + diff review |
| 6     | Folder regrouping (`attach/`, `sessions/`, `dialogs/`, `sparkline/`), barrels if wanted, docs refresh                                                                                   | low    | 4 gates               |

Phase 0 items are the only ones that change configuration; everything else is
mechanical. Note for Windows contributors: `npm run format:check` currently fails
on every CRLF working-tree file (see backlog item 21); fix that first or the gate
is unusable locally.

---

## 5. How we prove nothing changed

- **Diff shape**: after every step, `git diff -M --stat` should show renames and
  near-identical moves only. Any diff inside a moved body is a red flag.
- **Tests**: 243 unit tests + 14 Playwright specs run on every commit. For code with
  no coverage (replay `step()`, attach reconnect policy, session-list gestures),
  write characterization tests **before** the move, so the move is provably inert.
- **Type surface**: `tsc -b` is strict (`noUnusedLocals`, `noUnusedParameters`,
  `erasableSyntaxOnly`), so accidental signature drift fails the build.
- **Manual smoke list** (attach mode: terminal output, input, quick keys, hold
  repeat, upload; sessions page: search, filter, sort, group, pagination, pin,
  stop/kill optimistic state, mobile gestures; all dialogs; light + dark theme).

## 6. Definition of done

- [ ] No file in `src/` over 500 lines; pages under 400.
- [ ] Naming rules (§2) hold for every file; import specifiers uniform.
- [ ] `hooks/` exists and holds every shared hook; no hook lives inside a component file.
- [ ] `utils/` and `lib/` contain no imports from `components/` or `pages/`.
- [ ] Dead code from `web/ISSUES-BACKLOG.md` items 1–4 removed.
- [ ] `tsc -b`, `eslint`, `vitest`, `playwright` all green; docs (`FRONTEND.md`,
      `DESIGN.md`, `web/README.md`) updated to describe the final structure.

## 7. Not in scope

Behavior fixes, UX changes, new features, dependency upgrades, and anything listed
in `web/ISSUES-BACKLOG.md`.
