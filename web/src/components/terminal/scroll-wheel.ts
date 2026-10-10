/**
 * Wheel helpers for the terminal's mobile scroll handle: ownership, and where
 * a synthetic wheel should land.
 *
 * `SessionDetailPage` steps the log replay scrubber from a `wheel` listener on
 * the terminal container that is registered with `capture: true`. Capture
 * listeners on ancestors run *before* the event reaches the handle, so the
 * handle's own `stopPropagation` cannot help: one gesture over the handle would
 * both scroll the terminal and step the replay.
 *
 * Two kinds of wheel belong to the handle and must be ignored by such listeners:
 *   1. the real gesture, recognised by the cluster it started in, and
 *   2. the synthetic wheels XTerm dispatches into the terminal while scrolling
 *      the drag loop — tagged here because they are re-dispatched on xterm's
 *      own element, so the cluster check alone would not catch them.
 *
 * Those synthetic wheels carry coordinates too: with mouse tracking on
 * (`?1000`), xterm reports a wheel as the cell under its `clientX/clientY`, so
 * where they land decides which cell the program is told about — see
 * `wheelOrigin`.
 */

/** A rectangle in viewport coordinates; a `DOMRect` satisfies it. */
export interface Rect {
  left: number
  top: number
  width: number
  height: number
}

/**
 * The point inside `target` where the handle's synthetic wheel should land.
 *
 * The handle floats over the terminal, so its centre is the point the user is
 * actually gesturing on — reporting the target's centre would name the wrong
 * cell for every gesture. A handle with no box (it is `md:hidden` on desktop)
 * falls back to the centre, and the point is clamped into the target so a
 * letterboxed terminal always receives coordinates it owns.
 */
export function wheelOrigin(handle: Rect | null, target: Rect): { x: number; y: number } {
  const centre = { x: target.left + target.width / 2, y: target.top + target.height / 2 }
  if (!handle || handle.width <= 0 || handle.height <= 0) return centre
  return {
    x: Math.min(Math.max(handle.left + handle.width / 2, target.left), target.left + target.width),
    y: Math.min(Math.max(handle.top + handle.height / 2, target.top), target.top + target.height),
  }
}

/** Marks the control cluster that owns terminal scrolling gestures. */
export const SCROLL_CONTROLS_SELECTOR = '[data-terminal-scroll-controls]'

/** Synthetic wheels are tracked by identity: they are plain objects, not DOM. */
const ownedWheels = new WeakSet<WheelEvent>()

/** Tag a wheel that XTerm dispatches into the terminal on the user's behalf. */
export function markOwnedTerminalWheel(event: WheelEvent): void {
  ownedWheels.add(event)
}

/** True when the wheel belongs to the terminal's scroll handle, not the user. */
export function isOwnedTerminalWheel(event: WheelEvent): boolean {
  if (ownedWheels.has(event)) return true
  // Duck-type instead of `instanceof Element`: that would throw where `Element`
  // is undefined (SSR, unit tests) and fails across realms such as an iframe.
  const target = event.target as Element | null
  return typeof target?.closest === 'function' && target.closest(SCROLL_CONTROLS_SELECTOR) !== null
}
