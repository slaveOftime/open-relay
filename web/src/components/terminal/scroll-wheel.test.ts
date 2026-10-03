import { describe, expect, it } from 'vitest'

import {
  isOwnedTerminalWheel,
  markOwnedTerminalWheel,
  SCROLL_CONTROLS_SELECTOR,
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
