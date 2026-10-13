import { afterEach, beforeEach, describe, expect, it, vi, type Mock } from 'vitest'
import {
  FOLLOW_UP_CLICK_WINDOW_MS,
  REPEAT_DELAY_MS,
  REPEAT_INTERVAL_MS,
  RepeatController,
  type RepeatTimers,
} from './repeat-controller'

/**
 * The controller takes its timers so the whole hold lifecycle runs on fake
 * clocks without touching React.
 */
function fakeTimers(): RepeatTimers {
  return {
    setTimeout: (handler, ms) => setTimeout(handler, ms) as unknown as number,
    setInterval: (handler, ms) => setInterval(handler, ms) as unknown as number,
    clearTimeout: (id) => clearTimeout(id as unknown as ReturnType<typeof setTimeout>),
    clearInterval: (id) => clearInterval(id as unknown as ReturnType<typeof setInterval>),
  }
}

let controller: RepeatController
let action: Mock<() => void>

beforeEach(() => {
  vi.useFakeTimers()
  controller = new RepeatController(fakeTimers())
  action = vi.fn<() => void>()
})

afterEach(() => {
  controller.stop()
  vi.useRealTimers()
})

describe('RepeatController timing', () => {
  it('sends once immediately, then after the delay, then on the interval', () => {
    controller.start(action, { pointerId: 1, at: 0 })
    expect(action).toHaveBeenCalledTimes(1)

    vi.advanceTimersByTime(REPEAT_DELAY_MS)
    expect(action).toHaveBeenCalledTimes(2)

    vi.advanceTimersByTime(REPEAT_INTERVAL_MS * 3)
    expect(action).toHaveBeenCalledTimes(5)
  })

  it('stops the pending delay when released before it elapses', () => {
    controller.start(action, { pointerId: 1, at: 0 })
    controller.stop()
    vi.advanceTimersByTime(REPEAT_DELAY_MS + REPEAT_INTERVAL_MS * 2)
    expect(action).toHaveBeenCalledTimes(1)
  })

  it('stops a running interval when released', () => {
    controller.start(action, { pointerId: 1, at: 0 })
    vi.advanceTimersByTime(REPEAT_DELAY_MS + REPEAT_INTERVAL_MS)
    expect(action).toHaveBeenCalledTimes(3)

    controller.stop()
    vi.advanceTimersByTime(REPEAT_INTERVAL_MS * 5)
    expect(action).toHaveBeenCalledTimes(3)
  })

  it('cancels the previous hold when another starts', () => {
    const first = vi.fn<() => void>()
    const second = vi.fn<() => void>()
    controller.start(first, { pointerId: 1, at: 0 })
    controller.start(second, { pointerId: 2, at: 10 })

    expect(first).toHaveBeenCalledTimes(1)
    expect(second).toHaveBeenCalledTimes(1)

    vi.advanceTimersByTime(REPEAT_DELAY_MS + REPEAT_INTERVAL_MS)
    expect(first).toHaveBeenCalledTimes(1)
    expect(second).toHaveBeenCalledTimes(3)
  })
})

describe('RepeatController gesture matching', () => {
  it('ignores a key release that did not start the hold', () => {
    controller.start(action, { key: 'Enter', at: 0 })
    vi.advanceTimersByTime(REPEAT_DELAY_MS)
    expect(action).toHaveBeenCalledTimes(2)

    expect(controller.endsOnKey('Shift')).toBe(false)
    vi.advanceTimersByTime(REPEAT_INTERVAL_MS)
    expect(action).toHaveBeenCalledTimes(3)

    expect(controller.endsOnKey('Enter')).toBe(true)
    controller.stop()
  })

  it('ignores a pointer ending that did not start the hold', () => {
    controller.start(action, { pointerId: 4, at: 0 })
    vi.advanceTimersByTime(REPEAT_DELAY_MS)
    expect(action).toHaveBeenCalledTimes(2)

    // A second finger lands: another hold takes over (see the start case), and
    // the first finger lifting must not end it.
    controller.start(vi.fn<() => void>(), { pointerId: 7, at: 1 })
    expect(controller.endsOnPointer(4)).toBe(false)
    expect(controller.endsOnPointer(7)).toBe(true)
  })

  it('lets a pointer hold end on any key release', () => {
    controller.start(action, { pointerId: 1, at: 0 })
    expect(controller.endsOnKey('Shift')).toBe(true)
  })
})

describe('RepeatController click matching', () => {
  it('treats a click arriving with its gesture as a follow-up', () => {
    controller.start(action, { pointerId: 1, at: 1000 })
    controller.stop(1016)
    expect(controller.isFollowUpClick(1017)).toBe(true)
    expect(controller.isFollowUpClick(1016 + FOLLOW_UP_CLICK_WINDOW_MS)).toBe(true)
  })

  it('anchors the window to the release, not the press', () => {
    // A ten-second hold releases its click ten seconds after the press; if the
    // window were anchored to the press that click would send a second key.
    controller.start(action, { pointerId: 1, at: 0 })
    vi.advanceTimersByTime(10_000)
    expect(action.mock.calls.length).toBeGreaterThan(1)
    controller.stop(10_000)
    expect(controller.isFollowUpClick(10_005)).toBe(true)
  })

  it('treats a later (or synthetic) click as a first activation', () => {
    controller.start(action, { pointerId: 1, at: 1000 })
    controller.stop(1050)
    expect(controller.isFollowUpClick(1050 + FOLLOW_UP_CLICK_WINDOW_MS + 1)).toBe(false)
    // `timeStamp` 0 is what a hand-built event carries.
    expect(controller.isFollowUpClick(0)).toBe(false)
  })

  it('has no hold to match before the first activation', () => {
    const fresh = new RepeatController(fakeTimers())
    expect(fresh.isFollowUpClick(5000)).toBe(false)
  })
})
