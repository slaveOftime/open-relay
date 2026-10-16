import { describe, expect, it } from 'vitest'

import { DEFAULT_MENU_PLACEMENT, MENU_GAP_PX, MENU_INSET_PX, placeMenu } from './menu-placement'

/**
 * The rules a popup follows when its trigger can be dragged anywhere: under the
 * trigger while the room is there, over it when it is not, and slid sideways
 * when the trigger has run too close to the left edge of the screen. The
 * numbers are the phone viewport the control is built for (375x812), with the
 * menu at its usual 240x176 and the trigger at its default spot.
 */

const VIEWPORT = { width: 375, height: 812 }
const MENU = { width: 240, height: 176 }
/** `right-5 bottom-70`, the trigger's home: a 48px button 20px in from the right. */
const AT_HOME = { top: 484, right: 355, bottom: 532 }
/** Where the menu's left edge lands, margin box and inset included. */
const menuLeft = (anchorRight: number, placement: { right: number }) =>
  anchorRight - placement.right - MENU_INSET_PX - MENU.width

describe('placeMenu', () => {
  it('hangs the menu under the trigger when the room is there', () => {
    const placement = placeMenu(AT_HOME, MENU, VIEWPORT)

    expect(placement).toEqual(DEFAULT_MENU_PLACEMENT)
    expect(placement).toEqual({ vertical: 'below', right: 0 })
  })

  it('flips the menu over the trigger when the room under it is short', () => {
    // Dragged 170px down: 98px left below the trigger, against a 176px menu —
    // but 642px above it.
    const placement = placeMenu({ top: 654, right: 355, bottom: 702 }, MENU, VIEWPORT)

    expect(placement.vertical).toBe('above')
    // The right corner is still the aligned one: the flip is vertical only.
    expect(placement.right).toBe(0)
  })

  it('counts the gap and the margin when deciding the flip', () => {
    const fitsAt = VIEWPORT.height - MENU_GAP_PX - 4 - MENU.height

    expect(placeMenu({ top: 400, right: 355, bottom: fitsAt }, MENU, VIEWPORT).vertical).toBe(
      'below'
    )
    expect(placeMenu({ top: 400, right: 355, bottom: fitsAt + 1 }, MENU, VIEWPORT).vertical).toBe(
      'above'
    )
  })

  it('prefers the roomier side when the menu fits on neither', () => {
    const tall = { width: 240, height: 700 }

    // 452px below against 288px above.
    expect(placeMenu({ top: 300, right: 355, bottom: 348 }, tall, VIEWPORT).vertical).toBe('below')
    // 52px below against 688px above.
    expect(placeMenu({ top: 700, right: 355, bottom: 748 }, tall, VIEWPORT).vertical).toBe('above')
  })

  it('slides the menu back into the viewport when the trigger is at the left edge', () => {
    // Dragged to the clamp: the trigger's right edge leaves 52px to its left,
    // far short of the menu's 240.
    const placement = placeMenu({ top: 484, right: 52, bottom: 532 }, MENU, VIEWPORT)

    expect(placement.right).toBe(-208)
    // Which puts the menu's left edge exactly on the margin, and its right edge
    // well inside the viewport.
    expect(menuLeft(52, placement)).toBe(4)
    expect(52 - placement.right - MENU_INSET_PX).toBeLessThan(VIEWPORT.width)
  })

  it('never slides the menu left of its aligned corner', () => {
    // Whatever room there is, the menu only ever moves away from the trigger's
    // right edge, never past it.
    for (const right of [355, 375, 260, 244, 250]) {
      expect(placeMenu({ top: 484, right, bottom: 532 }, MENU, VIEWPORT).right).toBeLessThanOrEqual(
        0
      )
    }
  })

  it('keeps the left edge on screen even when the menu is wider than the viewport', () => {
    const placement = placeMenu({ top: 10, right: 100, bottom: 58 }, MENU, {
      width: 200,
      height: 812,
    })

    // Pinned to the margin: there is nothing better to do for the right edge.
    expect(placement.right).toBe(-160)
    expect(menuLeft(100, placement)).toBe(4)
  })

  it('leaves the menu alone when its left edge lands exactly on the margin', () => {
    // A menu whose left edge lands exactly on the margin is left aligned, not
    // slid: the boundary is inclusive, and it is the menu's own `mr-4` inset
    // that has to clear the trigger's right edge first.
    expect(placeMenu({ top: 484, right: 260, bottom: 532 }, MENU, VIEWPORT).right).toBe(0)
    expect(placeMenu({ top: 484, right: 259, bottom: 532 }, MENU, VIEWPORT).right).toBe(-1)
  })
})
