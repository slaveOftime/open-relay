import { afterEach, describe, expect, it, vi } from 'vitest'

import { SPARKLINE_BUCKET_MS, SparklineStore } from './sparklineStore'
import {
  calculateAverageBytesPerSecond,
  calculatePeakBytesPerSecond,
  calculateRecentBytesPerSecond,
  carryOpenBucket,
} from './sparklineMetrics'

afterEach(() => {
  vi.useRealTimers()
  vi.unstubAllGlobals()
})

describe('SparklineStore', () => {
  it('uses the first absolute total as a baseline only', () => {
    const store = new SparklineStore()

    store.recordTotal('session-1', 100)

    const series = store.getSeries('session-1')
    expect(series.every((value) => value === 0)).toBe(true)
  })

  it('records positive byte deltas into the active bucket', () => {
    const store = new SparklineStore()

    store.recordTotal('session-1', 100)
    store.recordTotal('session-1', 180)

    const series = store.getSeries('session-1')
    expect(series.at(-1)).toBe(80)
  })

  it('uses explicit previous totals when provided by events', () => {
    const store = new SparklineStore()

    store.recordTotal('session-1', 250, 200)

    const series = store.getSeries('session-1')
    expect(series.at(-1)).toBe(50)
  })

  it('tracks the last real output separately from bucket ticks and stale totals', () => {
    vi.useFakeTimers()
    vi.setSystemTime(1_000)
    const store = new SparklineStore()
    store.recordTotal('session-1', 100)
    expect(store.getLastOutputAt('session-1')).toBeNull()
    vi.setSystemTime(1_125)
    store.recordTotal('session-1', 180)
    expect(store.getLastOutputAt('session-1')).toBe(1_125)
    expect(store.getBucketIndex('session-1')).toBe(2)
    vi.setSystemTime(1_700)
    store.recordTotal('session-1', 120)
    expect(store.getLastOutputAt('session-1')).toBe(1_125)
    expect(store.getBucketIndex('session-1')).toBe(3)
  })
  it('ignores non-increasing totals', () => {
    const store = new SparklineStore()

    store.recordTotal('session-1', 300)
    store.recordTotal('session-1', 300)
    store.recordTotal('session-1', 250)

    const series = store.getSeries('session-1')
    expect(series.every((value) => value === 0)).toBe(true)
  })

  it('does not count old out-of-order totals twice', () => {
    const store = new SparklineStore()
    store.recordTotal('session-1', 100)
    store.recordTotal('session-1', 180)
    store.recordTotal('session-1', 120) // older REST response
    store.recordTotal('session-1', 190)
    expect(store.getSeries('session-1').at(-1)).toBe(90)
  })

  it('catches up an unmounted chart without polling and keeps its history intact', () => {
    vi.useFakeTimers()
    vi.setSystemTime(0)
    vi.stubGlobal('window', { setInterval: setInterval, clearInterval: clearInterval })
    const store = new SparklineStore()
    store.recordTotal('session-1', 100)
    store.recordTotal('session-1', 180)
    // The decay timer keeps running once any data is recorded, so the bucket
    // window keeps advancing even when no UI is mounted. This is what keeps
    // the visible history from snapping to zeros when the user navigates
    // away and back.
    expect(vi.getTimerCount()).toBe(1)

    vi.setSystemTime(21_000)
    const listener = vi.fn()
    const unsubscribe = store.subscribe('session-1', listener)
    // The bucket window has fully expired, so the visible series is now zero.
    expect(listener).toHaveBeenCalledTimes(1)
    expect(store.getSeries('session-1').every((value) => value === 0)).toBe(true)
    // Subscribing again shouldn't have disturbed the running timer.
    expect(vi.getTimerCount()).toBe(1)
    unsubscribe()
  })
  it('keeps bucket windows advancing while no UI is mounted', () => {
    vi.useFakeTimers()
    vi.setSystemTime(0)
    vi.stubGlobal('window', { setInterval: setInterval, clearInterval: clearInterval })
    const store = new SparklineStore()
    const listener = vi.fn()
    const unsubscribe = store.subscribe('local', listener)
    // Move the clock forward to a non-zero bucket index so the assertions
    // below describe behaviour during a realistic session lifecycle.
    vi.setSystemTime(1_500)
    store.recordTotal('local', 100)
    store.recordTotal('local', 300)
    const indexAfterRecord = store.getBucketIndex('local')
    expect(indexAfterRecord).toBeGreaterThan(0)
    // User navigates to a different page; subscription is torn down. The
    // decay timer keeps running so the bucket window keeps advancing — the
    // alternative was wiping history to zeros on next mount.
    unsubscribe()
    expect(vi.getTimerCount()).toBe(1)

    vi.setSystemTime(3_500)
    vi.advanceTimersByTime(2_500)
    expect(store.getBucketIndex('local')).toBeGreaterThan(indexAfterRecord)

    // Re-subscribe (the user came back). New data is still added to the
    // latest bucket, and gets exposed via subscribe listeners.
    const next = vi.fn()
    const off = store.subscribe('local', next)
    const before = store.getSeries('local')
    store.recordTotal('local', 500)
    expect(next).toHaveBeenCalled()
    expect(store.getSeries('local')).not.toBe(before)
    off()
  })
  it('notifies only the changed session and decays output every 500 ms, not idle rows', () => {
    vi.useFakeTimers()
    vi.setSystemTime(0)
    vi.stubGlobal('window', { setInterval: setInterval, clearInterval: clearInterval })
    const store = new SparklineStore()
    const first = vi.fn()
    const second = vi.fn()
    const unsubscribeFirst = store.subscribe('session-1', first)
    const unsubscribeSecond = store.subscribe('session-2', second)
    expect(vi.getTimerCount()).toBe(0)

    store.recordTotal('session-1', 100)
    store.recordTotal('session-1', 180)
    expect(vi.getTimerCount()).toBe(1)
    expect(first).toHaveBeenCalledTimes(1)
    expect(second).not.toHaveBeenCalled()
    const previous = store.getSeries('session-1')
    expect(previous).toBe(store.getSeries('session-1'))

    vi.advanceTimersByTime(SPARKLINE_BUCKET_MS)
    expect(first).toHaveBeenCalledTimes(2)
    expect(second).not.toHaveBeenCalled()
    expect(store.getSeries('session-1')).not.toBe(previous)
    expect(store.getSeries('session-1').at(-2)).toBe(80)
    expect(store.getSeries('session-1').at(-1)).toBe(0)

    vi.advanceTimersByTime(40 * SPARKLINE_BUCKET_MS)
    expect(store.getSeries('session-1').every((value) => value === 0)).toBe(true)
    // The data has aged out — the timer self-stops until new activity arrives.
    expect(vi.getTimerCount()).toBe(0)
    const callsAfterDecay = first.mock.calls.length
    vi.advanceTimersByTime(2 * SPARKLINE_BUCKET_MS)
    expect(first).toHaveBeenCalledTimes(callsAfterDecay)
    expect(second).not.toHaveBeenCalled()
    unsubscribeFirst()
    unsubscribeSecond()
    expect(vi.getTimerCount()).toBe(0)
  })
})

describe('sparkline rates', () => {
  it('uses a 2-second recent window and a 1-second peak over 500 ms buckets', () => {
    const series = new Array(40).fill(0)
    series[36] = 100
    series[38] = 100
    expect(calculateRecentBytesPerSecond(series)).toBe(100)
    expect(calculatePeakBytesPerSecond(series)).toBe(100)
    expect(calculateAverageBytesPerSecond(series)).toBe(10)
    series[36] = 0
    series[38] = 0
    expect(calculateRecentBytesPerSecond(series)).toBe(0)
  })
})

describe('sparkline display', () => {
  it('continues the previous point through a newly opened empty bucket', () => {
    const samples = [0, 120, 0]
    expect(carryOpenBucket(samples)).toEqual([0, 120, 120])
    expect(samples).toEqual([0, 120, 0]) // measured rates stay unchanged
    expect(carryOpenBucket([0, 120, 60])).toEqual([0, 120, 60])
    expect(carryOpenBucket([120, 0, 0])).toEqual([120, 0, 0])
    expect(carryOpenBucket([])).toEqual([])
  })
})
