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
│   ├── sessions/    session row, card, tags, pin, notification toggle, skeletons
│   ├── sparkline/   the sparkline renderer and its pure model
│   └── terminal/    XTerm wrapper, quick keys, fonts, theme, scroll wheel
├── hooks/         shared React hooks
├── lib/           domain modules and stateful services
├── pages/         route composition + page-local pure modules
└── utils/         pure framework-agnostic helpers
```

`components/` also holds a handful of singletons that belong to no group:
`Logo`, `CommandLogo`, `StatusBadge`, `NodeSelector`, `SseStatusDot`,
`NotificationToggle`, `ErrorBoundary`.

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

## Avoid

- Recreating overlays or modal shells with raw `fixed inset-0` markup when
  `Dialog` or `AlertDialog` already fits.
- Hand-rolling repeated label/help/error stacks in each dialog.
- Encoding important semantic styling directly in pages when a shared variant
  would work.
- One-off spacing/color rules that make a retheme require page-by-page edits.
- A hook that lives inside a component's file, where the next component that
  needs it will not find it.
