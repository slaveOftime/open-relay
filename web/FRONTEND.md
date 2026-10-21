# Frontend guide

## Goals

- Keep UI styling **theme-token driven** so rethemes happen in shared primitives, not page files.
- Prefer **shared components** in `src/components/ui` and reusable app components in `src/components/<group>`.
- Add new primitives only when a pattern repeats or is clearly semantic.
- Treat `DESIGN.md` as the source of truth for colors, density, shape, and retheming guidance.

## Structure

```
src/
├── api/           transport: REST client, attach WebSocket, SSE, shared types
├── components/
│   ├── ui/          Radix-backed primitives (Button, Dialog, Badge, ...)
│   ├── attach/      the attach drawer + its storage, history, send path, key bar
│   ├── dialogs/     every dialog, plus the helpers they share
│   ├── sessions/    session row, card, group header, tags, pin, notification toggle, skeletons
│   ├── sparkline/   the sparkline renderer and its pure model
│   └── terminal/    XTerm wrapper, quick keys, fonts, theme, scroll wheel
├── hooks/         shared React hooks
├── lib/           domain modules and stateful services
├── pages/         route composition + page-local pure modules
└── utils/         pure framework-agnostic helpers
```

`components/` also holds a handful of singletons that belong to no group:
`Logo`, `CommandLogo`, `StatusBadge`, `NodeSelector`, `SseStatusDot`,
`NotificationToggle`, `KeyInputRulesHint`, `ErrorBoundary`. `KeyInputRulesHint`
owns the popup that explains the key composer's rules (space counting, named
keys, combos) wherever a composer appears. `StatusBadge` composes a separate
`ProgramStatusBadge` for optional OSC 7501 app state. When a report exists,
render only the program badge: the report replaces the lifecycle label, while
the process lifecycle remains available from the API. Without a report, the
lifecycle badge remains authoritative, including **Input Needed** when the
backend `input_needed` attention flag is set. A non-running process must never
display ready/input-needed, even if a stale flag arrives. These are plain-text,
untrusted reports from the owning session, not verified completion or approval
authority. Program-state colors use dedicated `program-*`
Badge variants and `--program-status-*` theme tokens rather than lifecycle
variants, so retheming reports does not change process status badges.
`sameSessionSummary` must compare their fields so status-only SSE updates reach
rows, cards, and details.

## Conventions

- Components are `PascalCase.tsx`; every other module is `kebab-case.ts`.
- Tests sit next to the module they test and share its name
  (`use-repeat-while-pressed.test.ts`). A test named after one function, or
  after a module that no longer exists, is a bug in the tree's map.
- Imports use `@/…` when crossing a folder boundary, relative paths only for
  siblings, and never carry a file extension.
- Layers: `utils/` imports nothing internal; `lib/` and `hooks/` never import
  `components/` or `pages/`; `components/` never imports `pages/`.
- Size: `max-lines` warns at 500 for pages and components, 400 for modules.
  Treat a warning as a signal to split, not as noise to suppress.

## Where logic lives

| Kind                             | Home                          | Example                                                |
| -------------------------------- | ----------------------------- | ------------------------------------------------------ |
| Pure helper                      | `utils/`                      | `key-input.ts`, `session-ordering.ts`, `format.ts`     |
| Domain module / stateful service | `lib/`                        | `session-events.ts`, `push.ts`, `sparkline-store.ts`   |
| Page-local logic                 | `pages/*.ts` next to its page | `sessions-page-loaders.ts`, `session-detail-output.ts` |
| Shared hook                      | `hooks/`                      | `use-session-events.ts`, `use-attach-reconnect.ts`     |
| Frame/API translation table      | next to what it serves        | `attach-socket-options.ts`, `ws-frames.ts`             |

Put branch-heavy logic in a module rather than inline in a component, so it is
unit-testable without React and so the component reads as composition.

## Design rules

Follow the compact dark system in `DESIGN.md`: 32px default controls, 6-8px
radii, quiet near-black surfaces, hairline borders, and Open Relay green as the
primary accent.

1. **Start from existing primitives.** Use `Button`, `Input`, `Textarea`,
   `Badge`, `Dialog`, `AlertDialog`, `Tooltip`, `Select`, `Card`, `Table`, and
   `Slider` before writing raw elements. Reuse app components like
   `StatusBadge`, `SessionActionConfirmDialog`, `AttachPanel`, and
   `ImagePreviewDialog` when behavior matches.
2. **Use semantic variants before custom classes.** `Button` variants
   (`default`, `secondary`, `ghost`, `stop`, `kill`), `Badge` variants
   (`accent`, `warning`, `running`, `stopping`, `failed`). If the same custom
   styling appears twice, move it into a shared variant or component.
3. **Use theme tokens, not file-local colors.** `hsl(var(--background))`,
   `--foreground`, `--muted`, `--border`, `--primary`, `--destructive`, and
   `--scrollbar-thumb` for custom scroll areas. Avoid hard-coded page colors
   like `text-red-500` unless the token system cannot express the state yet.
4. **Keep dialogs consistent.** `Dialog` for standard modals, `AlertDialog` for
   confirm/destructive flows. Special layout goes on top of `DialogContent`,
   not a rebuilt overlay.
5. **Keep forms consistent.** `FormField`, `FormDescription`, `FormError`, and
   `FormActions` for dialog forms; `Input`/`Textarea` for controls.

## When adding new UI

1. Check whether an existing app component already matches the behavior.
2. If not, compose from `ui/*` primitives first.
3. If repeated styling shows up, extract a reusable app component.
4. If the missing piece is a standard accessible primitive, add the Radix-based
   wrapper under `src/components/ui`, matching the existing style: token
   colors, shared radius/border/focus ring, `cn(...)` merging, and exported
   subcomponents that mirror the Radix API.

## Live events

- One `subscribeSessionEvents` subscription per page mount. Read current
  filters from a ref instead of resubscribing on every filter, search, or page
  change; resubscribing churns the store's retain count and cancels pending
  background reloads (see `sessions-page-events.ts`).
- The hooks live in `hooks/use-session-events.ts`; the store and its imperative
  API in `lib/session-events.ts`.
- Connection state for `SseStatusDot`: `connecting` (first handshake, benign),
  `live`, `reconnecting` (a stream that opened before has dropped and retries
  are backing off), `offline`.

## Relative times

- `hooks/use-now.ts` is the shared clock behind a live label ("10s ago" on a
  session card). One interval serves every subscriber and runs only while
  something is subscribed; the reading is cached, because
  `useSyncExternalStore` compares snapshots by identity and a value read fresh
  per call would re-render forever.
- Subscribe in the label, not in the row: `SessionAge` renders the clock-backed
  count for a live session and a plain `formatTimestamp` for a finished one, so
  a list of finished sessions runs no timer.

## Terminal theme

- `index.css` resolves the scheme in two steps: an explicit `data-theme` on
  `<html>` wins, otherwise `prefers-color-scheme`. `components/terminal/xterm-theme.ts`
  mirrors that resolution and owns the subscription to both sources; `XTerm`
  only repaints when it fires, so an OS flip or a runtime override rethemes a
  live terminal instead of waiting for a remount.

## Lessons — the ones that cost a debugging session

- **Unstable hook dependencies.** An object literal passed into a hook and
  listed in a `useCallback` dependency array is a new function every render,
  and any effect keyed on it re-runs forever. `exhaustive-deps` catches a
  _missing_ dependency, not a _fresh_ one — there is no lint guard. Take flat
  inputs, and hold objects a hook builds itself in `useRef`.
- **Only the browser catches some regressions.** tsc, eslint and the unit suite
  were all green while a second Terminal mounted, a header label vanished, and
  a stale response was applied. If a change touches effect timing, mounts, or
  DOM focus, run the e2e suite — and check the spec still asserts what you
  think it does, because a swallowed assertion passes silently.
- **A state update that stops being called is invisible.** When reworking
  handlers, ask what each `setX` drives. The only signal is `no-unused-vars` on
  the setter.
- **Move verbatim, verify by comparison.** Extract by comparing the moved text
  against a `git worktree` copy of HEAD; eyeballing a diff misses lines. Beware
  shell round-trips too: PowerShell writes literal `\n` and re-encodes, which
  once swallowed two test assertions.
- **Test-first for wide seams.** Extract the pure function first
  (`attach-reconnect-policy.ts`, `runSessionLoad`), test it with fakes, then
  wire it up. A hook with ~25 threaded dependencies is a rewrite, not a move —
  and a rewrite needs a test before the code changes.
- **Annotate a sample with the type.** `session-summary.test.ts` types its
  sample `: SessionSummary` and its mutation table
  `Record<keyof SessionSummary, ...>`, so a field added to the type fails the
  build until it is considered. Cheap, and it stops silent drift.
- **Read a module's own tests before writing tests against it.** Three wrong
  tests came from assuming `parseKeySpec` returns null for an unknown key, when
  it throws.
- **The child carrying `truncate` is the child that pays.** The session detail
  header's metadata row had exactly one shrinkable item, so a long session id
  collapsed to its first character while every badge kept its size. Decide "this
  row never squeezes" on the container — `overflow-x-auto` plus
  `[&>*]:shrink-0` — and drop the `truncate` that was doing the damage.
- **A fixture that regenerates a timestamp holds a relative label still.**
  `sessions.spec.ts` reads "Ns ago" off a live card; the page re-fetches the
  list, so a `created_at` computed inside the route handler was rebuilt on every
  request and the count never moved — indistinguishable from a clock that never
  ticked. Build fixtures once, then hand them to the handler.
- **A sticky header must out-paint the rows it covers.** The sessions list's
  group header is `sticky top-0 z-20` on an opaque wrapper, because the header
  tint is translucent and the mobile card content carries `z-10` of its own: it
  needs an opaque layer beneath _and_ a level above the rows, in both the card
  list and the table (`e2e/sessions-group-collapse.spec.ts` hit-tests the band,
  so only a header that is actually on top passes).
- **A size remembered at connect time is stale by the time the socket opens.**
  The attach URL carries the terminal's viewport, but `sendResize` is a no-op
  until the socket is OPEN and `afterOpen` cleared the pending size; xterm only
  calls back on the _next_ change, so a refit that landed while connecting (the
  info bar and the webfont settling) left the PTY drawing for the handshake
  size until the page was re-entered. Reconcile against the terminal's current
  size when the socket opens — the native client already did.
  `e2e/attach-resize.spec.ts` holds a mocked socket in CONNECTING while the
  viewport changes, so the lost report is deterministic rather than a race.
- **An automatic view switch must replace its history entry, not push one.**
  When a session ended while attached, the page pushed the logs URL over the
  attach URL; Back then stepped onto the attach URL, which reconnected, ended
  again, and switched right back — the sessions list became unreachable.
  Anything the app does on its own (session end, a fallback view) belongs in
  the entry it replaces; only user-initiated navigation earns a new one.
- **Restore the list once the rows are real, and only the visible scrollport.**
  Collapsed groups are a `localStorage` preference (a reload keeps them), while
  the scroll offset is `sessionStorage` and is applied after the rows render —
  a `scrollTop` write against the loading skeleton just clamps. The hidden list
  is `display:none` and would clamp the same write to 0, so the restore picks
  the element with an `offsetParent`. The offset is read once per mount and the
  restore retries until it lands, because a foreground reload blanks the list
  while it runs — taking the rows away cancels the pending frame, and the
  collapsed content clamps the old offset to 0. Scrolls seen while the list has
  no rows are that clamp, not the user, and are not saved. `sessions-scroll.spec.ts`
  covers both scrollports and the interrupting reload.
- **A full-height layout has nothing to scroll, so the keyboard can cover it.**
  `useTerminalKeyboardSync` reveals a focused terminal by scrolling its nearest
  scroll container, but the session detail shell is already `h-dvh` with the
  terminal pinned to the bottom: `scrollBy` is a no-op and the IME paints over
  the cursor row. The layout viewport does not shrink for the keyboard, so
  `hooks/use-soft-keyboard-inset.ts` measures the `visualViewport` overlap and
  the page reserves that strip at the bottom; FitAddon then drops rows and
  reports the smaller grid to the PTY. Differences below a keyboard's size are
  browser chrome and must not trigger a refit.
  `e2e/attach-soft-keyboard-viewport.spec.ts` fakes the viewport height, holds
  the terminal's last row above the keyboard, and types through the refit.
- **A hidden textarea is not an append-only log.** xterm reads a soft keyboard
  by diffing `.xterm-helper-textarea`, and the diff (`newValue.replace(oldValue, '')`)
  only handles appends: an auto-paired quote, or a correction that edits in
  place, makes it miss and re-send the whole buffer on every later key — the
  reported `"""d"` on Android. `components/terminal/soft-keyboard-input.ts`
  keeps the textarea empty on touch platforms (cancel the insertion, send its
  data once, sweep silent residue before xterm samples the next key) and
  defers its post-composition clear past xterm's 0ms read of the textarea;
  `e2e/attach-soft-keyboard.spec.ts` holds the report.

## Avoid

- Recreating overlays or modal shells with raw `fixed inset-0` markup when
  `Dialog` or `AlertDialog` already fits.
- Hand-rolling repeated label/help/error stacks in each dialog.
- Encoding important semantic styling directly in pages when a shared variant
  would work.
- One-off spacing/color rules that make a retheme require page-by-page edits.
- A hook that lives inside a component's file, where the next component that
  needs it will not find it.
