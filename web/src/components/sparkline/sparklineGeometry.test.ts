import { describe, expect, it } from 'vitest'

import { buildSparklineModel } from './sparklineModel'
import { RUNNING_PALETTE, sparklinePalette } from './sparklinePalettes'
import {
  buildAreaPath,
  buildSmoothLinePath,
  buildSparklinePoints,
  sparklinePointsFromHeights,
} from './sparklineGeometry'

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
