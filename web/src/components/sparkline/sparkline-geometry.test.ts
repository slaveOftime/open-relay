import { describe, expect, it } from 'vitest'

import { buildSparklineModel } from './sparkline-model'
import { RUNNING_PALETTE, sparklinePalette } from './sparkline-palettes'
import {
  SPARKLINE_FADE_MS,
  SPARKLINE_HOLD_MS,
  advanceTimedBuckets,
  animatedHeadY,
  buildAreaPath,
  buildSmoothLinePath,
  buildSparklinePoints,
  heldHeadY,
  seedHeldBuckets,
  sparklinePointsFromHeights,
  timedSparklinePoints,
} from './sparkline-geometry'

describe('buildSparklinePoints', () => {
  it('draws a flat baseline when the series is too short to plot', () => {
    // A single sample has no shape, and the baseline keeps the chart anchored.
    const points = buildSparklinePoints([7], 100, 20)

    expect(points).toEqual([
      { x: 0, y: 17 },
      { x: 100, y: 17 },
    ])
  })

  it('spaces points evenly across the width and scales y by log10 emphasis', () => {
    const points = buildSparklinePoints([0, 10, 1000], 90, 20)

    // Log scaling: 10 fills ~1/3 of the range, 1000 fills all of it. SVG y
    // grows downward, so a taller bar is a smaller y.
    expect(points).toHaveLength(3)
    expect(points.map((point) => point.x)).toEqual([0, 45, 90])
    expect(points[1].y).toBeLessThan(17)
    expect(points[2].y).toBeLessThan(points[1].y)
    // Every point stays inside the padding box (top 2, bottom 3).
    for (const point of points) {
      expect(point.y).toBeGreaterThanOrEqual(2)
      expect(point.y).toBeLessThanOrEqual(17)
    }
  })

  it('collapses a flat series onto the baseline rather than dividing by zero', () => {
    const points = buildSparklinePoints([0, 0, 0], 60, 20)

    expect(points.every((point) => point.y === 17)).toBe(true)
  })
})

describe('sparklinePointsFromHeights', () => {
  it('keeps the given heights and spaces them by bucket index', () => {
    expect(sparklinePointsFromHeights([4, 10], 40)).toEqual([
      { x: 0, y: 4 },
      { x: 40, y: 10 },
    ])
  })

  it('uses the first height as a flat fallback with no history', () => {
    expect(sparklinePointsFromHeights([6], 30)).toEqual([
      { x: 0, y: 6 },
      { x: 30, y: 6 },
    ])
  })
})

describe('buildSmoothLinePath', () => {
  it('emits one move and one line per point with two decimals', () => {
    expect(
      buildSmoothLinePath([
        { x: 0, y: 1.005 },
        { x: 10.5, y: 2 },
      ])
    ).toBe('M 0.00 1.00 L 10.50 2.00')
  })

  it('returns an empty string with no points', () => {
    expect(buildSmoothLinePath([])).toBe('')
  })
})

describe('buildAreaPath', () => {
  it('closes the area back to the baseline', () => {
    const points = [
      { x: 0, y: 5 },
      { x: 10, y: 2 },
    ]

    // The line's leading "M" is sliced off so the area can chain onto it, which
    // leaves a cosmetic double space in the middle of the path.
    expect(buildAreaPath(points, 17)).toBe(
      'M 0.00 17.00 L 0.00 5.00  0.00 5.00 L 10.00 2.00 L 10.00 17.00 Z'
    )
  })

  it('reuses an already-built line path', () => {
    const points = [
      { x: 0, y: 5 },
      { x: 10, y: 2 },
    ]
    const line = buildSmoothLinePath(points)

    expect(buildAreaPath(points, 17, line)).toBe(buildAreaPath(points, 17))
  })
})

describe('buildSparklineModel', () => {
  it('picks the palette from the running state', () => {
    expect(buildSparklineModel([1, 2], 80, 22, true).palette).toEqual(RUNNING_PALETTE)
    expect(buildSparklineModel([1, 2], 80, 22, false).palette).toEqual(sparklinePalette(false))
  })

  it('falls back to the baseline for a series with nothing to plot', () => {
    const model = buildSparklineModel([0], 80, 22, false)

    expect(model.lastPoint).toEqual({ x: 80, y: 19 })
    expect(model.baselineY).toBe(19)
  })

  it('carries the same baseline into the area and the points', () => {
    const model = buildSparklineModel([0, 5, 50], 90, 22, true)

    expect(model.points).toHaveLength(3)
    expect(model.linePath).toContain('M 0.00')
    expect(model.areaPath.endsWith('Z')).toBe(true)
    expect(model.baselineY).toBe(Math.max(2, 22 - 3))
  })
})

describe('continuous sparkline timeline', () => {
  it('moves existing heights without waiting for a new store snapshot', () => {
    const heights = [19, 2, 11]
    const first = timedSparklinePoints(heights, 250, 500, 20, 11)
    const later = timedSparklinePoints(heights, 350, 500, 20, 11)
    expect(later[1].x).toBeLessThan(first[1].x)
    expect(later[1].y).toBe(first[1].y)
    expect(later.at(-1)).toEqual({ x: 20, y: 11 })
  })

  it('keeps sample coordinates continuous as a wall-clock bucket rolls over', () => {
    const original = [19, 2, 11]
    const before = timedSparklinePoints(original, 499, 500, 20, 11)
    const advanced = advanceTimedBuckets(original, 0, 1, 500, 100, 11, 19)
    const after = timedSparklinePoints(advanced, 500, 500, 20, 11)
    expect(advanced).toEqual([2, 11, 11])
    expect(after[0].y).toBe(before[1].y)
    expect(after[0].x).toBeCloseTo(before[1].x, 1)
    expect(after[1].y).toBe(before[2].y)
    expect(after[1].x).toBeCloseTo(before[2].x, 1)
  })

  it('restores a held tail when mounted after the last output', () => {
    const original = [19, 2, 19, 19, 19]
    const seeded = seedHeldBuckets(original, 1, 4, 500, 750, 19)
    expect(seeded).toEqual([
      19,
      2,
      heldHeadY(1000, 750, 2, 19),
      heldHeadY(1500, 750, 2, 19),
      heldHeadY(2000, 750, 2, 19),
    ])
    expect(original).toEqual([19, 2, 19, 19, 19])
    expect(seedHeldBuckets(original, 1, 10, 500, 750, 19).at(-1)).toBe(19)
  })
  it('continues scrolling old activity after the live head fades', () => {
    const initial = [19, 19, 19, 19, 2]
    const after1200 = advanceTimedBuckets(initial, 0, 2, 500, 100, 2, 19)
    const after1900 = advanceTimedBuckets(after1200, 2, 3, 500, 100, 2, 19)
    const pointAt1200 = timedSparklinePoints(after1200, 1200, 500, 20, heldHeadY(1200, 100, 2, 19))
    const pointAt1900 = timedSparklinePoints(after1900, 1900, 500, 20, heldHeadY(1900, 100, 2, 19))
    expect(pointAt1200.find((point) => point.y === 2)?.x).toBeGreaterThan(
      pointAt1900.find((point) => point.y === 2)?.x ?? Infinity
    )
    expect(pointAt1900.at(-1)?.y).toBe(heldHeadY(1900, 100, 2, 19))
  })
  it('eases only the live head, then holds and fades that measurement', () => {
    expect(animatedHeadY(100, 19, 2, 100, 100, 19, 220)).toBe(19)
    // easeOutCubic at 50% progress covers 87.5% of the distance.
    expect(animatedHeadY(210, 19, 2, 100, 100, 19, 220)).toBeCloseTo(4.125, 3)
    expect(animatedHeadY(210, 19, 2, 100, 100, 19, 220)).toBeLessThan(19)
    expect(animatedHeadY(100 + SPARKLINE_HOLD_MS, 2, 2, 100, 100, 19, 220)).toBe(2)
    expect(
      animatedHeadY(100 + SPARKLINE_HOLD_MS + SPARKLINE_FADE_MS, 2, 2, 100, 100, 19, 220)
    ).toBe(19)
  })
  it('holds for the configured duration then linearly fades to baseline', () => {
    expect(heldHeadY(SPARKLINE_HOLD_MS, 0, 2, 19)).toBe(2)
    expect(heldHeadY(SPARKLINE_HOLD_MS + SPARKLINE_FADE_MS / 2, 0, 2, 19)).toBe(10.5)
    expect(heldHeadY(SPARKLINE_HOLD_MS + SPARKLINE_FADE_MS, 0, 2, 19)).toBe(19)
    const original = [19, 2, 19, 19, 19]
    const carried = advanceTimedBuckets(original, 0, 3, 500, 100, 2, 19)
    expect(carried.slice(-3)).toEqual([500, 1000, 1500].map((t) => heldHeadY(t, 100, 2, 19)))
    expect(original).toEqual([19, 2, 19, 19, 19])
  })
})
