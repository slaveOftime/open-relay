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

  it('catches up an unmounted chart without polling or keeping its timer alive', () => {
    vi.useFakeTimers()
    vi.setSystemTime(0)
    vi.stubGlobal('window', { setInterval })
    const store = new SparklineStore()
    store.recordTotal('session-1', 100)
    store.recordTotal('session-1', 180)
    expect(vi.getTimerCount()).toBe(0)

    vi.setSystemTime(21_000)
    const listener = vi.fn()
    const unsubscribe = store.subscribe('session-1', listener)
    expect(listener).toHaveBeenCalledTimes(1)
    expect(store.getSeries('session-1').every((value) => value === 0)).toBe(true)
    unsubscribe()
    expect(vi.getTimerCount()).toBe(0)
  })
  it('notifies only the changed session and decays output every 500 ms, not idle rows', () => {
    vi.useFakeTimers()
    vi.setSystemTime(0)
    vi.stubGlobal('window', { setInterval })
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
