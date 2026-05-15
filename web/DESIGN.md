# Open Relay frontend design system

Version: beta

Open Relay uses a compact, dark, terminal-adjacent interface. The visual system should feel like a developer control plane: dense but readable, token-driven, and easy to retheme from a small set of HSL CSS variables.

## Design principles

1. **Compact by default**: controls use 32px default height, tight 6-8px radii, and 12-16px panel padding.
2. **Dark canvas, quiet chrome**: most surfaces sit between near-black canvas and slightly elevated cards. Borders are hairline and low contrast.
3. **Green as the product accent**: primary actions, links, active states, and attach affordances use the existing Open Relay green.
4. **Semantic state colors only**: warning, destructive, info, and success colors appear only for state communication.
5. **Retheme through tokens**: prefer `hsl(var(--...))` tokens and shared component variants over page-local color classes.
6. **Radix-backed primitives**: use existing wrappers in `src\components\ui` before custom DOM; add Radix wrappers for common accessible patterns.

## Token palette

The app uses HSL tokens in `src\index.css`.

| Token | Dark value | Purpose |
| --- | --- | --- |
| `--background` | `220 18% 3%` | app canvas |
| `--foreground` | `210 20% 92%` | primary text |
| `--card` | `220 14% 6%` | cards, dialogs, menus |
| `--popover` | `220 14% 6%` | floating surfaces |
| `--secondary` | `220 11% 10%` | inputs, secondary buttons |
| `--muted` | `220 11% 10%` | passive panels |
| `--muted-foreground` | `220 8% 66%` | helper text |
| `--accent` | `220 10% 14%` | hover/active neutral |
| `--primary` | `160 84% 39%` | Open Relay green |
| `--destructive` | `0 72% 48%` | destructive state |
| `--border` | `220 9% 16%` | hairline borders |
| `--input` | `220 9% 16%` | input borders |
| `--ring` | `160 84% 39%` | focus rings |
| `--radius` | `0.375rem` | default compact radius |

Light mode exists for accessibility/system preference, but new design work should optimize the compact dark theme first and keep light values token-compatible.

## Typography

- Use the system sans stack already defined in `src\index.css`.
- Base text: `14px / 1.45`.
- Labels/captions: `11-12px`, muted, slight letter spacing only when needed.
- Headings in dialogs/cards should usually be `16-18px`, not marketing-scale sizes.
- Use `font-mono` only for IDs, paths, commands, terminal data, and machine values.

## Shape, spacing, density

| Item | Standard |
| --- | --- |
| Control height | 32px default, 28px small |
| Icon button | 32px square |
| Radius | 6px default, 8px cards/dialogs, full only for pills |
| Panel padding | 12-16px |
| Form field gap | 6px inside field, 12-16px between fields |
| Table row density | prefer `py-2` to `py-3` |

Avoid `rounded-xl`, large shadows, and `p-6` unless the element is intentionally spacious.

## Components

### Buttons

Use `Button` variants:

- `default`: primary green CTA.
- `secondary`: neutral elevated action.
- `outline`: low-emphasis bordered action.
- `ghost`: toolbar/icon actions.
- `link`: inline navigation/action.
- `stop` / `kill`: session lifecycle actions.

Do not hard-code hover colors for primary buttons. The shared button variant owns hover/focus styling.

### Forms

Use:

- `Input` for single-line text.
- `Textarea` for multiline text.
- `FormField`, `FormDescription`, `FormError`, `FormActions` for dialog forms.

Avoid hand-written label/help/error stacks in feature components.

### Dialogs

Use:

- `Dialog` for standard modal workflows.
- `AlertDialog` for confirmation or destructive flows.
- `SessionActionConfirmDialog` for stop/kill session actions.

Fullscreen media viewers may customize `DialogContent`, but should still use the shared `Dialog` shell and token-compatible chrome.

### Badges

Use semantic `Badge` variants:

- `accent` for node/tag/product-accent metadata.
- `warning` for offline or degraded state.
- status variants from `StatusBadge` for session state.

Avoid page-local emerald/amber/red badge recipes; add a variant when a semantic style repeats.

### Cards, menus, and popovers

Cards, dropdowns, select menus, tooltips, dialogs, and alert dialogs should use card/popover tokens and compact shadows. Keep borders visible but quiet.

## Adding new UI

1. Check `src\components\ui` for an existing primitive.
2. Compose an app-level component under `src\components` if behavior is domain-specific.
3. Add a Radix wrapper under `src\components\ui` when the missing pattern is generic and accessible.
4. Use HSL tokens and variants, not hard-coded color recipes.
5. Update this file and `FRONTEND.md` when adding a new primitive or semantic variant.

## Anti-patterns

- Raw modal overlays when `Dialog` or `AlertDialog` fits.
- New hard-coded colors in page files.
- Large marketing-style spacing in operational UI.
- Repeated form field markup.
- One-off shadows/radii that diverge from the compact system.
