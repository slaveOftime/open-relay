import { describe, expect, it } from 'vitest'

import { MIN_SOFT_KEYBOARD_INSET_PX, softKeyboardInset } from './soft-keyboard'

describe('softKeyboardInset', () => {
  it('returns the part of the layout viewport the keyboard covers', () => {
    expect(softKeyboardInset(844, { height: 520, offsetTop: 0 })).toBe(324)
  })

  it('ignores browser chrome that is smaller than a keyboard', () => {
    expect(
      softKeyboardInset(844, { height: 844 - (MIN_SOFT_KEYBOARD_INSET_PX - 1), offsetTop: 0 })
    ).toBe(0)
  })

  it('treats a keyboard exactly at the threshold as open', () => {
    expect(softKeyboardInset(844, { height: 844 - MIN_SOFT_KEYBOARD_INSET_PX, offsetTop: 0 })).toBe(
      MIN_SOFT_KEYBOARD_INSET_PX
    )
  })

  it('accounts for a visual viewport panned below the layout viewport top', () => {
    // The visible strip is 100..700, so the keyboard starts at 700 and the app
    // has to end there, not at 700 - 100.
    expect(softKeyboardInset(844, { height: 600, offsetTop: 100 })).toBe(144)
  })

  it('clamps a visual viewport larger than the layout viewport to zero', () => {
    expect(softKeyboardInset(844, { height: 900, offsetTop: 0 })).toBe(0)
  })

  it('rounds up so the last row is never left under the keyboard', () => {
    expect(softKeyboardInset(844, { height: 520.2, offsetTop: 0 })).toBe(324)
  })

  it('returns zero without a usable visual viewport', () => {
    expect(softKeyboardInset(844, null)).toBe(0)
    expect(softKeyboardInset(844, { height: 0, offsetTop: 0 })).toBe(0)
    expect(softKeyboardInset(0, { height: 500, offsetTop: 0 })).toBe(0)
  })
})
