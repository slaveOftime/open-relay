/**
 * Wheel ownership for the terminal's mobile scroll handle.
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
 */

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
