/**
 * How much of the layout viewport the soft keyboard currently covers.
 *
 * `window.innerHeight` (the layout viewport) does not shrink when a mobile
 * browser opens the IME: by default the keyboard is painted over the visual
 * viewport. `window.visualViewport` is the only reliable report of the part
 * that is still visible, so the difference is the strip a terminal must keep
 * its cursor out of — otherwise a terminal pinned to the bottom row renders
 * the cursor underneath the keyboard.
 *
 * Small differences are browser chrome (the URL bar animating in and out), not
 * a keyboard, and shifting the body for those would make the layout jump on
 * every scroll.
 */

/**
 * The smallest overlap treated as an IME. A phone keyboard is several hundred
 * pixels; a collapsing URL bar is a few dozen.
 */
export const MIN_SOFT_KEYBOARD_INSET_PX = 80

export interface VisualViewportMetrics {
  height: number
  /** Offset of the visual viewport from the layout viewport's top edge. */
  offsetTop: number
}

export function softKeyboardInset(
  layoutHeight: number,
  viewport: VisualViewportMetrics | null | undefined
): number {
  if (!viewport || !Number.isFinite(viewport.height) || viewport.height <= 0) return 0
  if (!Number.isFinite(layoutHeight) || layoutHeight <= 0) return 0

  const visibleBottom = (viewport.offsetTop || 0) + viewport.height
  const inset = layoutHeight - visibleBottom
  if (!Number.isFinite(inset) || inset < MIN_SOFT_KEYBOARD_INSET_PX) return 0
  // Round up: leaving one extra pixel above the keyboard is harmless, while
  // leaving the bottom row one pixel under it is the bug being fixed.
  return Math.ceil(inset)
}

/**
 * The smallest gap to leave between the cursor row and the IME.
 *
 * The row itself is enough to see the character being typed; a little air
 * keeps the cursor from touching the keyboard's top edge on subpixel layouts.
 */
export const SOFT_KEYBOARD_CURSOR_MARGIN_PX = 8

export interface SoftKeyboardShiftMetrics {
  /** The full keyboard overlap reported by `softKeyboardInset`. */
  keyboardInset: number
  /** The cursor's row from the top of xterm's viewport, 0-based. */
  cursorRow: number | null
  /** The number of rows in xterm's viewport. */
  rows: number
  /** The terminal screen's top edge before the page shift is applied. */
  terminalTop: number
  /** The terminal screen's height. */
  terminalHeight: number
  /** The bottom edge of the area the keyboard leaves visible. */
  visibleBottom: number
  /** Space to leave above the keyboard; defaults to the cursor margin. */
  margin?: number
}

/**
 * How far to shift the page so the cursor stays above the soft keyboard.
 *
 * The keyboard can cover the bottom of the view, but it must not push a cursor
 * that is already visible above it out of the top of the screen. So this is
 * the smallest shift that brings the cursor row above the IME, clamped to the
 * keyboard overlap: a cursor in the middle or near the top needs no shift at
 * all, while a cursor near the bottom only moves the page as far as necessary.
 */
export function softKeyboardShift({
  keyboardInset,
  cursorRow,
  rows,
  terminalTop,
  terminalHeight,
  visibleBottom,
  margin = SOFT_KEYBOARD_CURSOR_MARGIN_PX,
}: SoftKeyboardShiftMetrics): number {
  if (!(keyboardInset > 0) || cursorRow === null || !(rows > 0) || !(terminalHeight > 0)) {
    return 0
  }
  if (!Number.isFinite(cursorRow) || cursorRow < 0 || cursorRow >= rows) return 0
  if (!Number.isFinite(terminalTop) || !Number.isFinite(visibleBottom)) return 0

  const rowHeight = terminalHeight / rows
  const cursorBottom = terminalTop + (cursorRow + 1) * rowHeight
  const needed = cursorBottom + margin - visibleBottom
  if (!Number.isFinite(needed) || needed <= 0) return 0

  return Math.min(keyboardInset, Math.ceil(needed))
}
