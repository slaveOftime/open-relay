/**
 * Placement for a popup that hangs off a floating trigger — the sessions
 * search control's filter menu. The trigger can be dragged anywhere the page
 * frame allows, so the menu cannot simply be pinned under its right corner: it
 * is measured when it opens (and again whenever it changes size or the window
 * does) and told where to go.
 *
 * Pure, so the rules are testable without a browser: hang under the trigger
 * when the room is there, flip over it when it is not, and slide the menu
 * sideways when the trigger has left less than the menu's width between its own
 * right edge and the edge of the screen.
 */
import { EDGE_MARGIN_PX, type Size } from './floating-position'

/** The trigger's box, in viewport coordinates. */
export interface AnchorBox {
  top: number
  right: number
  bottom: number
}

/** Where the menu goes, and how far it had to move to stay on screen. */
export interface MenuPlacement {
  /** `above` flips the menu over the trigger's top edge. */
  vertical: 'below' | 'above'
  /**
   * CSS `right` offset from the trigger's own box, in px. `0` puts the menu's
   * right edge on the trigger's; anything negative slides it right, far enough
   * to bring its left edge back to the margin. Never positive — the menu grows
   * away from the trigger's right edge, and there is no reason to move it
   * closer in.
   */
  right: number
}

/** The menu's clearance from the trigger. Mirrors the `mt-2`/`mb-2` class. */
export const MENU_GAP_PX = 8

/** Used before the first measurement, and whenever the menu is closed. */
export const DEFAULT_MENU_PLACEMENT: MenuPlacement = { vertical: 'below', right: 0 }

export function placeMenu(
  anchor: AnchorBox,
  menu: Size,
  viewport: Size,
  margin: number = EDGE_MARGIN_PX
): MenuPlacement {
  // Right-aligned with the trigger, growing leftward, unless that would push
  // the menu's left edge past the margin — then it is slid back to the margin,
  // however far right of the trigger that lands. A menu wider than the screen
  // can only be helped at one end, so the left one wins.
  const right = Math.min(0, anchor.right - menu.width - margin)

  // Under the trigger, unless the menu does not fit there — then over it, if
  // that is where the room is. With too little room either way, the bigger side
  // wins rather than the menu hanging off the nearer edge.
  const below = viewport.height - anchor.bottom - MENU_GAP_PX - margin
  const above = anchor.top - MENU_GAP_PX - margin
  if (below >= menu.height) return { vertical: 'below', right }
  if (above >= menu.height) return { vertical: 'above', right }
  return { vertical: below >= above ? 'below' : 'above', right }
}
