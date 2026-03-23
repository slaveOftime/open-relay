import { describe, expect, it } from 'vitest'

import { SparklineStore } from './SparklineSvg'

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
})
