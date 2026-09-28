import { describe, expect, it } from 'vitest'
import { easeOutCubic, interpolateSparklinePoints, sameSparklinePoints } from './sparklineGeometry'

describe('sparkline geometry animation', () => {
  const before = [
    { x: 0, y: 19 },
    { x: 80, y: 19 },
  ]
  const after = [
    { x: 0, y: 2 },
    { x: 80, y: 10 },
  ]

  it('interpolates positions without modifying the measured endpoints', () => {
    expect(interpolateSparklinePoints(before, after, 0.5)).toEqual([
      { x: 0, y: 10.5 },
      { x: 80, y: 14.5 },
    ])
    expect(before[0].y).toBe(19)
    expect(after[0].y).toBe(2)
  })

  it('can skip redundant frames when the curve has not moved', () => {
    expect(
      sameSparklinePoints(
        before,
        before.map((point) => ({ ...point }))
      )
    ).toBe(true)
    expect(sameSparklinePoints(before, after)).toBe(false)
    expect(sameSparklinePoints(before, [{ x: 0, y: 19 }])).toBe(false)
    expect(easeOutCubic(0)).toBe(0)
    expect(easeOutCubic(1)).toBe(1)
    expect(easeOutCubic(0.5)).toBeGreaterThan(0.5)
  })
})
