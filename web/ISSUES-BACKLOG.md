# Web backlog — findings only

Everything here was found while refactoring/investigating in Oct 2026. Items 1-5
were fixed by the refactor itself (they were gates or dead code, not behavior) and
are marked **fixed** with the commit; the rest are still open, so the refactor
could stay behavior-preserving and the work can be scheduled deliberately.
Format: `area — finding — why it matters — suggested next step`.

## Correctness / test-infra

1. **FIXED** (`5e0f426`) — `src/components/StatusBadge.test.tsx` never ran. `vite.config.ts` sets
   `test.include: ['src/**/*.test.ts']`, which does not match `.test.tsx`, so the one
   component-level test in the repo is silently skipped (`npm test` reports 30 files,
   not 31).
   _Next step:_ widen the glob to `src/**/*.test.{ts,tsx}` and re-check the count.

2. **FIXED** (`5e0f426`) — `npx eslint .` linted build output. `eslint.config.js` ignores `dist` but not
   `dev-dist`, and the PWA build emits `dev-dist/workbox-*.js`, which fails with
   "Definition for rule '@typescript-eslint/ban-types' was not found" plus 3 unused
   `eslint-disable` warnings. CI only passes because `dev-dist` doesn't exist there.
   _Next step:_ add `dev-dist` to `globalIgnores`.

3. **OPEN, and it now blocks work** — Playwright has no trace/screenshot/retry setup: `retries: 0`, `reporter: 'list'`,
   no `use.trace`, no projects (so no WebKit/iOS run). The mobile focus-ring bug fixed
   in `59c473e` only reproduces on WebKit, which the suite never runs — that is why it
   shipped. _Next step:_ add `trace: 'on-first-retry'`, `screenshot: 'only-on-failure'`,
   and a `webkit` (or `Mobile Safari`) project in CI.

4. **`vitest` runs with `environment: 'node'` and no `jsdom`/`happy-dom` installed,
   so component/DOM tests cannot be added without new dependencies. All current tests
   are pure modules (good), but it limits future coverage of things like the
   hold-repeat interactions. _Next step:_ decide deliberately, not accidentally.

## Dead code / duplication

5. **FIXED** (`5e0f426`) — three unused UI primitives (~213 lines) were deleted: `components/ui/sheet.tsx` (120),
   `toggle-group.tsx` (70), `separator.tsx` (23). Nothing imports them; `Radix` deps
   `react-separator`, `react-toggle-group`, and the sheet's `react-dialog` (still used
   by `dialog.tsx`) could then be dropped from `package.json`.
   _Next step:_ delete, or wire them in if a real use exists.

6. **FIXED** (`6ce694d`) — `findScrollContainer` was duplicated almost verbatim in `components/XTerm.tsx`
   (382–399) and `components/AttachPanel.tsx` (268–285). They can drift.
   _Next step:_ extract to `utils/scroll-container.ts` (already in the refactor plan).

7. **FIXED** (`6ce694d`) — `SparklineSvg.tsx` re-implemented geometry that exists in
   `sparklineGeometry.ts`. Two sources of truth for the same math.
   _Next step:_ dedupe during Phase 4.

8. **FIXED** (`d08f32d`) — commented-out code: the import block in `pages/SessionDetailPage.tsx` (45–51) and
   the "Attach Error dialog" block (1915–1947). _Next step:_ delete; history keeps it.

## Structure / layering

9. **FIXED** (`07585b8`) — layer inversion between `lib/` and `components/`: `lib/quick-keys-storage.ts`
   imports `components/terminal/quick-keys`, and `lib/session-activity.ts` imports
   `sparklineStore.ts` (in `lib/`), while components import back from `lib`. A data module
   depending on the view layer blocks reuse and complicates testing.
   _Next step:_ move `DEFAULT_QUICK_KEYS`/encoding into `lib/`, the sparkline store into
   `components/`, or introduce a shared `model/` module.

10. **FIXED** (`d08f32d`) — `pages/SessionsPage.tsx` re-exported a UI primitive (`export { Badge }`,
    line 2514, "Needed for Badge import"). A page acting as a component registry.
    _Next step:_ import `Badge` from `@/components/ui/badge` where it is needed.

11. **FIXED** (`d08f32d`, `e1053c9`) — import-specifier drift: `NodeSelector.tsx` uses `@/api/types.ts` and
    `@/components/ui/select.tsx` (with extensions) while the other ~164 `@/` imports
    omit them; several files mix relative and aliased imports for the same target
    (`App.tsx`, `XTerm.tsx`, `AttachPanel.tsx`, `SessionDetailPage.tsx`).
    _Next step:_ pick one rule (plan §2) and enforce it in lint.

12. **FIXED** (`cd81413`) — two "utils" roots read inconsistently: `@/lib/utils` (`cn`) vs
    `@/utils/*` (domain helpers). Three files import both (`CommandLogo.tsx`,
    `NewSessionDialog.tsx`, `StatusBadge.tsx`). _Next step:_ move `cn` to
    `utils/cn.ts` (Phase 2).

13. **FIXED** (`c0aeaa8`) — no home for hooks: `use-reduced-motion.ts` and `use-repeat-while-pressed.ts`
    live in `components/terminal/`, and `lib/session-events.ts` exports both a
    non-hook imperative API and three hooks. _Next step:_ `src/hooks/` (Phase 2).

14. **`api/types.ts` mirrors the Rust protocol by hand** ("mirrors Rust protocol.rs").
    Drift risk between daemon and client. _Next step:_ consider codegen from
    `src/protocol.rs`, or a CI check that compares field lists.

## UX / product notes (deliberate decisions, not bugs)

15. **Quick keys have no focus ring any more.** After `59c473e` (park focus on press
    to keep the soft keyboard down), the AttachPanel quick keys also suppress the
    `focus-visible` ring, so keyboard users lose the visible affordance on those
    buttons. _Next step:_ if we want it back, mark pointer-parked focus with a data
    attribute instead of a blanket class.

16. **Radial quick keys are `md:hidden`** (mobile only); desktop users only get the
    AttachPanel grid. Intended, but worth keeping in mind when the panels diverge.

17. **`SessionsPage` desktop table and mobile cards are two parallel implementations**
    of the same row content (`SessionRow` vs `SessionCard`), which is why they drift
    (e.g. `flex-row-reverse` differences). _Next step:_ share the cell/field
    presentational pieces, keep the two shells.

18. **Quick-keys grid scrolling on touch** (`max-h-27`, `overflow-y-auto`) interacts
    with `touch-action`; `touch-none` was removed from the keys to restore drag
    scrolling, so a hold that starts to scroll cancels the repeat via `pointercancel`.
    Probably fine, but there is no test covering "scroll vs hold" on a key.
    _Next step:_ add an e2e gesture test if it ever misbehaves.

19. **FIXED** (`5e0f426`, `e1053c9`) — `web/README.md` was the Vite template. First impression for new
    contributors. _Next step:_ rewrite with the structure from
    `web/REFACTOR-PLAN.md` once Phase 6 lands.

20. **`package-lock.json` churn on every `npm` command** (registry URLs switch to a
    mirror). It keeps showing up as an unrelated modified file in commits.
    _Next step:_ pin `.npmrc` `registry` in the repo or ignore the lock diff during
    local dev.

21. **FIXED** (`5e0f426`) — `npm run format:check` used to fail locally on Windows for 105 files even though CI
    passes. Cause is line endings, not formatting: `core.autocrlf=true` checks out
    CRLF, and `.prettierrc` does not set `endOfLine`, so Prettier's `lf` default
    reports every CRLF file. On Linux CI the checkout is LF and the gate passes.
    _Next step:_ add `"endOfLine": "auto"` to `.prettierrc` — a no-op in CI (files are
    already LF there) and it makes the gate usable on Windows.

22. **`react-hooks/refs` violations that were masked by file size.** The rule
    reports render-phase ref access, but it stopped analysing
    `src/components/attach/AttachPanel.tsx` entirely while the file was large
    enough. Removing three small handlers from it brought it under the rule's
    analysis threshold and four pre-existing violations appeared at once: two
    `Ref.current = false` writes in the session-change branch, one reached
    through a state-setter that also writes a ref, and one in the quick-keys
    `map` callback. _Why it matters:_ the render-phase ref writes in the
    "adjust state on prop change" branch are a real React anti-pattern; they
    are load-bearing here (they must run before the persistence effects that
    read them, which an effect cannot do), so fixing them means reworking the
    persistence contract. _Next step:_ first audit the other large components -
    `SessionDetailPage.tsx`, `SessionsPage.tsx`, `XTerm.tsx`, `SparklineSvg.tsx`
    - for the same pattern, since the rule is silently skipping them today.
      The four sites are suppressed with an explanatory comment rather than
      changed, because changing them would alter persistence timing.
