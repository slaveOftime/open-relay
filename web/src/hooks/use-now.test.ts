import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { CLOCK_TICK_MS, getClockNow, subscribeClock } from './use-now'

/**
 * The clock is module state, so every test unsubscribes what it subscribed: a
 * listener left behind would keep the interval alive into the next one.
 */

beforeEach(() => {
  vi.useFakeTimers()
})

afterEach(() => {
  vi.useRealTimers()
})

describe('the shared clock', () => {
  it('ticks a subscriber once a second while it has one', () => {
    const ticks: number[] = []
    const unsubscribe = subscribeClock(() => ticks.push(getClockNow()))
    const start = getClockNow()

    vi.advanceTimersByTime(CLOCK_TICK_MS * 3)

    expect(ticks).toEqual([start + 1000, start + 2000, start + 3000])
    unsubscribe()
  })

  it('stops the interval with the last subscriber', () => {
    const ticks: number[] = []
    const first = subscribeClock(() => ticks.push(1))
    const second = subscribeClock(() => ticks.push(2))

    // Two consumers, one interval.
    expect(vi.getTimerCount()).toBe(1)
    vi.advanceTimersByTime(CLOCK_TICK_MS)
    expect(ticks).toEqual([1, 2])

    first()
    vi.advanceTimersByTime(CLOCK_TICK_MS)
    expect(ticks).toEqual([1, 2, 2])

    second()
    vi.advanceTimersByTime(CLOCK_TICK_MS)
    expect(ticks).toEqual([1, 2, 2])
    expect(vi.getTimerCount()).toBe(0)
  })

  it('catches the reading up when the first consumer arrives', () => {
    // Sync the reading with the clock, then let go: with nothing subscribed
    // there is no interval, and the reading goes as stale as the gap between
    // two mounts.
    const stop = subscribeClock(() => {})
    const synced = getClockNow()
    stop()

    vi.advanceTimersByTime(60_000)
    expect(getClockNow()).toBe(synced)

    const unsubscribe = subscribeClock(() => {})

    // The first consumer reads the wall clock again, so the render it is
    // subscribing for counts from now rather than from whenever the last
    // consumer left.
    expect(getClockNow()).toBe(synced + 60_000)
    unsubscribe()
  })

  it('keeps the reading stable between ticks', () => {
    const unsubscribe = subscribeClock(() => {})

    // Halfway through a tick: a snapshot that drifted here would re-render
    // every consumer in a loop.
    vi.advanceTimersByTime(CLOCK_TICK_MS / 2)
    const reading = getClockNow()
    expect(getClockNow()).toBe(reading)

    unsubscribe()
  })
})
