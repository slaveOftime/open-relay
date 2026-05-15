# Frontend guide

## Goals

- Keep UI styling **theme-token driven** so future rethemes mostly happen in shared primitives, not page files.
- Prefer **shared components** in `web\src\components\ui` and small reusable app components in `web\src\components`.
- Add new primitives only when a pattern is used in more than one place or is clearly semantic.
- Treat `web\DESIGN.md` as the product design source of truth for colors, density, shape, and retheming guidance.

## Current structure

- `src\components\ui\*`: shared UI primitives built on Radix or lightweight wrappers.
- `src\components\*`: app-level building blocks composed from the UI primitives.
- `src\pages\*`: page composition only; avoid page-local design systems.

## Design rules

Follow the compact dark system in `web\DESIGN.md`: 32px default controls, 6-8px radii, quiet near-black surfaces, hairline borders, and Open Relay green as the primary accent.

1. **Start from existing primitives**
   - Use `Button`, `Input`, `Textarea`, `Badge`, `Dialog`, `AlertDialog`, `Tooltip`, `Select`, `Card`, `Table`, and `Slider` before writing raw elements.
   - Reuse app components like `SessionActionConfirmDialog`, `StatusBadge`, `AttachPanel`, and `ImagePreviewDialog` when behavior matches.

2. **Use semantic variants before custom classes**
   - Prefer `Button` variants like `default`, `secondary`, `ghost`, `stop`, and `kill`.
   - Prefer `Badge` variants like `accent`, `warning`, `running`, `stopping`, `failed`, etc.
   - If the same custom styling appears twice, move it into a shared variant or component.

3. **Use theme tokens, not file-local colors**
   - Prefer classes based on `hsl(var(--background))`, `--foreground`, `--muted`, `--border`, `--primary`, `--destructive`, and related tokens.
   - Avoid hard-coded page colors like `text-red-500` or one-off emerald/amber blocks unless the token system cannot express the state yet.

4. **Keep dialogs consistent**
   - Use `Dialog` for standard modal flows.
   - Use `AlertDialog` for confirm/destructive flows.
   - If a dialog needs special layout, keep it on top of `DialogContent` instead of rebuilding the overlay/shell from scratch.

5. **Keep forms consistent**
   - Use `FormField`, `FormDescription`, `FormError`, and `FormActions` for dialog forms.
   - Use `Input`/`Textarea` for controls so sizing, borders, focus rings, and muted text stay aligned.

## When adding new UI

### Add a page feature

1. Check whether an existing app component already matches the behavior.
2. If not, compose from `ui/*` primitives first.
3. If repeated styling shows up, extract a reusable app component.
4. If the missing piece is a standard accessible primitive, add the Radix-based wrapper under `src\components\ui`.

### Add a new Radix primitive

1. Install the Radix package in `web`.
2. Create a wrapper in `src\components\ui`.
3. Match the existing wrapper style:
   - token-based colors
   - shared radius, border, and focus ring patterns
   - `cn(...)` merging
   - exported subcomponents that mirror the Radix API
4. Refactor existing code to use the new primitive where it replaces duplicated UI.

## File-level expectations

- **Pages** should mostly orchestrate data, routing, and composition.
- **App components** may contain behavior and layout, but should still consume shared primitives.
- **UI primitives** should be generic, theme-friendly, and low-level.

## Examples in this codebase

- `src\components\ui\alert-dialog.tsx`: shared confirm-dialog primitive built on Radix.
- `src\components\ui\textarea.tsx`: shared multiline input matching `Input`.
- `src\components\ui\form-field.tsx`: shared dialog-form field structure.
- `src\components\SessionActionConfirmDialog.tsx`: app-level wrapper for repeated stop/kill confirmation UI.

## Avoid

- Recreating overlays or modal shells with raw `fixed inset-0` markup when `Dialog` or `AlertDialog` already fits.
- Hand-rolling repeated label/help/error stacks in each dialog.
- Encoding important semantic styling directly in pages when a shared variant would work.
- Adding one-off spacing/color rules that make future rethemes require page-by-page edits.
