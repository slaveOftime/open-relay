import { describe, expect, it } from 'vitest'

import {
  isOwnedTerminalWheel,
  markOwnedTerminalWheel,
  SCROLL_CONTROLS_SELECTOR,
  wheelOrigin,
} from './scroll-wheel'

/**
 * The suite runs in the `node` environment (see vite.config.ts), so these build
 * the minimum event shape the helper inspects instead of real DOM nodes.
 */
function wheelFrom(target: unknown): WheelEvent {
  return { target } as unknown as WheelEvent
}

/** A target that reports the given `closest()` match, or none. */
function targetMatching(closestResult: string | null): Element {
  return { closest: () => closestResult } as unknown as Element
}

describe('isOwnedTerminalWheel', () => {
  it('leaves an ordinary terminal wheel alone', () => {
    expect(isOwnedTerminalWheel(wheelFrom(targetMatching(null)))).toBe(false)
  })

  it('claims a wheel that starts inside the control cluster', () => {
    expect(isOwnedTerminalWheel(wheelFrom(targetMatching('div')))).toBe(true)
  })

  it('claims a tagged synthetic wheel wherever it was dispatched', () => {
    // XTerm re-dispatches on xterm's own element, outside the cluster.
    const synthetic = wheelFrom(targetMatching(null))
    expect(isOwnedTerminalWheel(synthetic)).toBe(false)
    markOwnedTerminalWheel(synthetic)
    expect(isOwnedTerminalWheel(synthetic)).toBe(true)
  })

  it('tolerates a wheel with no usable target', () => {
    expect(() => isOwnedTerminalWheel(wheelFrom(null))).not.toThrow()
    expect(isOwnedTerminalWheel(wheelFrom(null))).toBe(false)
    expect(isOwnedTerminalWheel(wheelFrom({}))).toBe(false)
  })

  it('matches the attribute the terminal sets on its control cluster', () => {
    expect(SCROLL_CONTROLS_SELECTOR).toBe('[data-terminal-scroll-controls]')
  })
})

describe('wheelOrigin', () => {
  // Left/top are non-zero on purpose: coordinates are viewport-absolute, and a
  // rect at the origin would hide an accidental use of centre-relative maths.
  const target = { left: 100, top: 50, width: 400, height: 300 }

  it('falls back to the target centre when there is no handle', () => {
    expect(wheelOrigin(null, target)).toEqual({ x: 300, y: 200 })
  })

  it('falls back to the target centre when the handle has no box', () => {
    // The `md:hidden` handle on desktop still has a ref but no layout.
    expect(wheelOrigin({ left: 0, top: 0, width: 0, height: 0 }, target)).toEqual({
      x: 300,
      y: 200,
    })
  })

  it('lands on the handle centre when it floats over the target', () => {
    const handle = { left: 440, top: 300, width: 48, height: 48 }
    expect(wheelOrigin(handle, target)).toEqual({ x: 464, y: 324 })
  })

  it('clamps a centre that hangs off the near edge back inside', () => {
    const handle = { left: -20, top: 100, width: 48, height: 48 }
    expect(wheelOrigin(handle, target)).toEqual({ x: 100, y: 124 })
  })

  it('clamps a centre past the far edges back inside', () => {
    const handle = { left: 490, top: 340, width: 48, height: 48 }
    expect(wheelOrigin(handle, target)).toEqual({ x: 500, y: 350 })
  })
})
