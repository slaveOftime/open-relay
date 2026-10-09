import { describe, expect, it } from 'vitest'
import {
  SPARKLINE_HOLD_MS,
  SPARKLINE_FADE_MS,
  advanceTimedBuckets,
  animatedHeadY,
  heldHeadY,
  seedHeldBuckets,
  timedSparklinePoints,
} from './sparkline-geometry'

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
