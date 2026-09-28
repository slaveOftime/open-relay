import { describe, expect, it } from 'vitest'
import {
  animationEase,
  animationWindow,
  easeOutCubic,
  interpolateSparklinePoints,
  isBucketShift,
  sameSparklinePoints,
} from './sparklineGeometry'

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

  it('distinguishes a rolling bucket shift from a newly received byte total', () => {
    expect(isBucketShift([0, 100, 0], [100, 0, 0])).toBe(true)
    expect(isBucketShift([0, 100, 0], [100, 0, 30])).toBe(false)
    expect(isBucketShift([0, 100, 0], [0, 100, 30])).toBe(false)
    expect(isBucketShift([0, 100, 0], [100, 0])).toBe(false)
  })
  it('glides for the full bucket and keeps that deadline across output updates', () => {
    const shifted = animationWindow(1000, true, null, 500, 220)
    expect(shifted).toEqual({ duration: 500, scrollEnd: 1500 })
    expect(animationWindow(1250, false, shifted.scrollEnd, 500, 220)).toEqual({
      duration: 250,
      scrollEnd: 1500,
    })
    expect(animationWindow(1550, false, shifted.scrollEnd, 500, 220).duration).toBe(220)
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
    expect(animationEase(0.5, true)).toBe(0.5)
    expect(animationEase(0.5, false)).toBeGreaterThan(0.5)
  })
})
