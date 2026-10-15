import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { PointerEvent as ReactPointerEvent } from 'react'

import { DOUBLE_TAP_MS, SLOP_PX, TapGestures, type TapKind } from './use-tap-gestures'

/**
 * The handlers are a plain object of DOM callbacks, so the contract can be
 * exercised without a renderer: tap vs double tap, the slop that hands a
 * travelling press to the drag gesture, and the click suppression contract.
 * With a double action a single is classified only when the double window
 * closes, so the timers are fake and stepped explicitly; without one there is
 * no window at all.
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

/** A consumer with no double action — nothing for a single to wait for. */
function setupTapOnly() {
  const taps: TapKind[] = []
  const gestures = new TapGestures()
  gestures.setCallbacks({ onPress: () => taps.push('single') })
  return { taps, gestures }
}

beforeEach(() => {
  vi.useFakeTimers()
})

afterEach(() => {
  vi.useRealTimers()
})

describe('TapGestures', () => {
  it('holds a still press for the double window, then fires a single tap', () => {
    const { taps, gestures } = setup()
    const down = pressEvent()

    gestures.handlers.onPointerDown(down)
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))

    // Nothing yet: the window is still open for a possible pair.
    expect(taps).toEqual([])
    vi.advanceTimersByTime(DOUBLE_TAP_MS - 1)
    expect(taps).toEqual([])
    vi.advanceTimersByTime(1)
    expect(taps).toEqual(['single'])
    // The press's default (focusing the button) must be suppressed: it would
    // steal focus from an input the tap just gave it.
    expect(down.preventDefault).toHaveBeenCalled()
  })

  it('a fast second tap completes the pair, and the single never runs', () => {
    const { taps, gestures } = setup()

    gestures.handlers.onPointerDown(pressEvent())
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))
    gestures.handlers.onPointerDown(pressEvent({ timeStamp: 1150 }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1200 }))

    expect(taps).toEqual(['double'])
    // The cancelled single must not fire late.
    vi.advanceTimersByTime(DOUBLE_TAP_MS * 2)
    expect(taps).toEqual(['double'])
  })

  it('a second tap after the window is its own single', () => {
    const { taps, gestures } = setup()

    gestures.handlers.onPointerDown(pressEvent())
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))
    vi.advanceTimersByTime(DOUBLE_TAP_MS)
    gestures.handlers.onPointerDown(pressEvent({ timeStamp: 1500 }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1550 }))
    vi.advanceTimersByTime(DOUBLE_TAP_MS)

    expect(taps).toEqual(['single', 'single'])
  })

  it('a tap that moves within the slop still counts, and can complete a pair', () => {
    const { taps, gestures } = setup()

    gestures.handlers.onPointerDown(pressEvent())
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))
    gestures.handlers.onPointerDown(pressEvent({ timeStamp: 1100 }))
    gestures.handlers.onPointerMove(pressEvent({ timeStamp: 1120, clientX: SLOP_PX, clientY: 0 }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1150 }))

    expect(taps).toEqual(['double'])
  })

  it('a travelling press is not a tap, and cancels a single still waiting', () => {
    const { taps, gestures } = setup()

    gestures.handlers.onPointerDown(pressEvent())
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))
    // A drag starts: it takes over the gesture, wait and all.
    gestures.handlers.onPointerDown(pressEvent({ timeStamp: 1100 }))
    gestures.handlers.onPointerMove(
      pressEvent({ timeStamp: 1120, clientX: SLOP_PX + 5, clientY: 0 })
    )
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1150 }))
    vi.advanceTimersByTime(DOUBLE_TAP_MS * 2)

    expect(taps).toEqual([])
  })

  it('ignores secondary pointers and non-left buttons', () => {
    const { taps, gestures } = setup()

    gestures.handlers.onPointerDown(pressEvent({ isPrimary: false }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))
    gestures.handlers.onPointerDown(pressEvent({ button: 2 }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1100 }))
    vi.advanceTimersByTime(DOUBLE_TAP_MS * 2)

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
    vi.advanceTimersByTime(DOUBLE_TAP_MS * 2)

    expect(taps).toEqual([])
  })

  it('a second press that is cancelled leaves the first tap standing', () => {
    const { taps, gestures } = setup()

    gestures.handlers.onPointerDown(pressEvent())
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))
    gestures.handlers.onPointerDown(pressEvent({ timeStamp: 1100 }))
    gestures.handlers.onPointerCancel(pressEvent({ timeStamp: 1120 }))
    vi.advanceTimersByTime(DOUBLE_TAP_MS)

    // The cancelled press was never a tap, but the first one still is.
    expect(taps).toEqual(['single'])
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

describe('TapGestures without a double action', () => {
  it('fires the tap on release, with no window to wait out', () => {
    const { taps, gestures } = setupTapOnly()
    const down = pressEvent()

    gestures.handlers.onPointerDown(down)
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))

    expect(taps).toEqual(['single'])
    // And nothing is left waiting to fire later.
    vi.advanceTimersByTime(DOUBLE_TAP_MS * 2)
    expect(taps).toEqual(['single'])
    expect(down.preventDefault).toHaveBeenCalled()
  })

  it('counts a fast second tap as its own tap, not a pair', () => {
    const { taps, gestures } = setupTapOnly()

    gestures.handlers.onPointerDown(pressEvent())
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))
    gestures.handlers.onPointerDown(pressEvent({ timeStamp: 1100 }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1150 }))

    // A consumer with no double is free to treat two taps as two toggles — the
    // control that opens on the first and closes on the second.
    expect(taps).toEqual(['single', 'single'])
  })

  it('still cancels a travelling press', () => {
    const { taps, gestures } = setupTapOnly()

    gestures.handlers.onPointerDown(pressEvent())
    gestures.handlers.onPointerMove(pressEvent({ timeStamp: 1020, clientX: SLOP_PX + 5 }))
    gestures.handlers.onPointerUp(pressEvent({ timeStamp: 1050 }))

    expect(taps).toEqual([])
  })
})
