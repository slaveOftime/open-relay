import { describe, expect, it } from 'vitest'
import { easeOutCubic } from './sparklineGeometry'

describe('sparkline head easing', () => {
  it('starts and ends exactly while smoothing only the newest observation', () => {
    expect(easeOutCubic(0)).toBe(0)
    expect(easeOutCubic(1)).toBe(1)
    expect(easeOutCubic(0.5)).toBeGreaterThan(0.5)
  })
})
