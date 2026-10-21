/**
 * How much of the layout viewport the soft keyboard currently covers.
 *
 * `window.innerHeight` (the layout viewport) does not shrink when a mobile
 * browser opens the IME: by default the keyboard is painted over the visual
 * viewport. `window.visualViewport` is the only reliable report of the part
 * that is still visible, so the difference is the space the app must keep its
 * content out of — otherwise a terminal pinned to the bottom row renders the
 * cursor underneath the keyboard.
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
