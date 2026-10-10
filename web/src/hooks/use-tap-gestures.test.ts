import { describe, expect, it, vi } from 'vitest'
import type { PointerEvent as ReactPointerEvent } from 'react'

import { DOUBLE_TAP_MS, SLOP_PX, TapGestures, type TapKind } from './use-tap-gestures'

/**
 * The handlers are a plain object of DOM callbacks, so the contract can be
 * exercised without a renderer: tap vs double tap, the slop that hands a
 * travelling press to the drag gesture, and the click suppression contract.
 */

function pressEvent(overrides: Record<string, unknown> = {}): ReactPointerEvent<HTMLButtonElement> {
  return {
    button: 0,
    isPrimary: true,
    timeStamp: 1000,
    clientX: 0,
    clientY: 0,
    preventDefault: vi.fn(),
    ...overrides,
  } as unknown as ReactPointerEvent<HTMLButtonElement>
}

function setup(slopPx?: number, doubleMs?: number) {
  const taps: TapKind[] = []
  const gestures = new TapGestures(slopPx, doubleMs)
  gestures.setCallbacks({
    onPress: () => taps.push('single'),
    onDoubleTap: () => taps.push('double'),
  })
  return { taps, gestures }
}

describe('TapGestures', () => {
  it('releases a still press as a single tap, with the default suppressed', () => {
    const { taps, gestures } = setup()
    const down = pressEvent()

    gestures.handlers.onPointerDown(down)
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))

    expect(taps).toEqual(['single'])
    // The press's default (focusing the button) must not steal focus from an
    // input the tap just gave it.
    expect(down.preventDefault).toHaveBeenCalled()
  })

  it('completes the pair when the second tap lands fast', () => {
    const { taps, gestures } = setup()

    gestures.handlers.onPointerDown(pressEvent())
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))
    gestures.handlers.onPointerDown(pressEvent({ timeStamp: 1150 }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1200 }))

    // The first tap already fired; the second completes the pair instead of
    // firing its own single.
    expect(taps).toEqual(['single', 'double'])
  })

  it('treats a slower second tap as its own single', () => {
    const { taps, gestures } = setup(undefined, DOUBLE_TAP_MS)

    gestures.handlers.onPointerDown(pressEvent())
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))
    gestures.handlers.onPointerDown(pressEvent({ timeStamp: 1500 }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1550 }))

    expect(taps).toEqual(['single', 'single'])
  })

  it('spends the pair on a double: the next tap starts a new one', () => {
    const { taps, gestures } = setup()

    for (const [down, up] of [
      [1000, 1050],
      [1100, 1150],
      [1250, 1300],
    ]) {
      gestures.handlers.onPointerDown(pressEvent({ timeStamp: down }))
      gestures.handlers.onPointerUp(pressEvent({ timeStamp: up }))
    }

    expect(taps).toEqual(['single', 'double', 'single'])
  })

  it('a press at exactly the double window is still a pair', () => {
    const { taps, gestures } = setup()

    gestures.handlers.onPointerDown(pressEvent())
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1000 }))
    gestures.handlers.onPointerDown(pressEvent({ timeStamp: 1000 + DOUBLE_TAP_MS }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1000 + DOUBLE_TAP_MS }))

    expect(taps).toEqual(['single', 'double'])
  })

  it('a travelling press is not a tap, and it breaks a pending pair', () => {
    const { taps, gestures } = setup()

    gestures.handlers.onPointerDown(pressEvent())
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))
    // A drag starts: the press belongs to the move now.
    gestures.handlers.onPointerDown(pressEvent({ timeStamp: 1100 }))
    gestures.handlers.onPointerMove(
      pressEvent({ timeStamp: 1120, clientX: SLOP_PX + 5, clientY: 0 })
    )
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1150 }))
    expect(taps).toEqual(['single'])
    // A tap arriving soon after the drag must not complete the broken pair.
    gestures.handlers.onPointerDown(pressEvent({ timeStamp: 1200 }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1230 }))

    expect(taps).toEqual(['single', 'single'])
  })

  it('a press within the slop is still a tap', () => {
    const { taps, gestures } = setup()

    gestures.handlers.onPointerDown(pressEvent())
    gestures.handlers.onPointerMove(pressEvent({ timeStamp: 1020, clientX: SLOP_PX, clientY: 0 }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))

    expect(taps).toEqual(['single'])
  })

  it('ignores secondary pointers and non-left buttons', () => {
    const { taps, gestures } = setup()

    gestures.handlers.onPointerDown(pressEvent({ isPrimary: false }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))
    gestures.handlers.onPointerDown(pressEvent({ button: 2 }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1100 }))

    expect(taps).toEqual([])
  })

  it('a press that leaves the button, or is cancelled, is not a tap', () => {
    const { taps, gestures } = setup()

    gestures.handlers.onPointerDown(pressEvent())
    gestures.handlers.onPointerLeave(pressEvent({ timeStamp: 1020 }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))
    gestures.handlers.onPointerDown(pressEvent({ timeStamp: 1100 }))
    gestures.handlers.onPointerCancel(pressEvent({ timeStamp: 1120 }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1150 }))

    expect(taps).toEqual([])
  })

  it('anchors the follow-up click window to the release', () => {
    const { gestures } = setup()

    gestures.handlers.onPointerDown(pressEvent())
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))

    expect(gestures.isFollowUpClick(1050)).toBe(true)
    expect(gestures.isFollowUpClick(1500)).toBe(true)
    expect(gestures.isFollowUpClick(1600)).toBe(false)
    // A click from before the gesture is a first activation (assistive tech).
    expect(gestures.isFollowUpClick(1040)).toBe(false)
  })
})
