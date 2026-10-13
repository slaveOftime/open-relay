import { describe, expect, it } from 'vitest'

import {
  clampPosition,
  EDGE_MARGIN_PX,
  loadPosition,
  normalizePosition,
  savePosition,
} from './floating-position'
import type { StoragePort } from './floating-position'

const KEY = 'oly.terminal.scrollHandle.v1'

/** A storage stand-in; the module takes a port, so no global stubbing is needed. */
function storage(
  initial: Record<string, string> = {}
): StoragePort & { data: Map<string, string> } {
  const data = new Map(Object.entries(initial))
  return {
    getItem: (key: string) => (data.has(key) ? data.get(key)! : null),
    setItem: (key: string, value: string) => void data.set(key, value),
    data,
  }
}

/** Same shape, but every operation throws — private mode, or PWA quota. */
function brokenStorage(): StoragePort {
  return {
    getItem: () => {
      throw new Error('unavailable')
    },
    setItem: () => {
      throw new Error('quota')
    },
  }
}

describe('normalizePosition', () => {
  it('keeps a valid offset pair', () => {
    expect(normalizePosition({ x: 12, y: 340.5 })).toEqual({ x: 12, y: 340.5 })
  })

  it('rejects non-numeric coordinates', () => {
    expect(normalizePosition({ x: '12', y: 340 })).toBeNull()
    expect(normalizePosition({ x: 12 })).toBeNull()
    expect(normalizePosition(null)).toBeNull()
    expect(normalizePosition([1, 2])).toBeNull()
  })

  it('rejects values a screen coordinate can never be', () => {
    expect(normalizePosition({ x: Number.NaN, y: 0 })).toBeNull()
    expect(normalizePosition({ x: 0, y: Number.POSITIVE_INFINITY })).toBeNull()
  })
})

describe('loadPosition', () => {
  it('returns the stored position', () => {
    const port = storage({ [KEY]: '{"x":100,"y":200}' })

    expect(loadPosition(KEY, port)).toEqual({ x: 100, y: 200 })
  })

  it('returns null when nothing is stored', () => {
    expect(loadPosition(KEY, storage())).toBeNull()
  })

  it('returns null for corrupt or wrong-shaped entries', () => {
    expect(loadPosition(KEY, storage({ [KEY]: 'not json' }))).toBeNull()
    expect(loadPosition(KEY, storage({ [KEY]: '{"x":true,"y":2}' }))).toBeNull()
    expect(loadPosition(KEY, storage({ [KEY]: '"a string"' }))).toBeNull()
  })

  it('returns null when storage itself fails', () => {
    expect(loadPosition(KEY, brokenStorage())).toBeNull()
  })

  it('returns null when there is no storage at all', () => {
    // Node (the test environment) and locked-down browsers have none.
    expect(loadPosition(KEY)).toBeNull()
  })
})

describe('savePosition', () => {
  it('writes JSON the loader can read back', () => {
    const port = storage()

    savePosition(KEY, { x: 7, y: 9 }, port)

    expect(port.data.get(KEY)).toBe('{"x":7,"y":9}')
    expect(loadPosition(KEY, port)).toEqual({ x: 7, y: 9 })
  })

  it('swallows a failing storage instead of throwing mid-drag', () => {
    expect(() => savePosition(KEY, { x: 1, y: 2 }, brokenStorage())).not.toThrow()
  })
})

describe('clampPosition', () => {
  const element = { width: 48, height: 48 }
  const container = { width: 375, height: 812 }

  it('leaves a position that is already inside untouched', () => {
    expect(clampPosition({ x: 100, y: 200 }, element, container)).toEqual({ x: 100, y: 200 })
  })

  it('pulls a position back inside the container', () => {
    expect(clampPosition({ x: -50, y: 900 }, element, container)).toEqual({
      x: EDGE_MARGIN_PX,
      y: 812 - 48 - EDGE_MARGIN_PX,
    })
    // Only the offending axis moves; an inside coordinate is left alone.
    expect(clampPosition({ x: 500, y: 5 }, element, container)).toEqual({
      x: 375 - 48 - EDGE_MARGIN_PX,
      y: 5,
    })
  })

  it('pins at the margin when the container cannot fit the element', () => {
    const tiny = { width: 100, height: 100 }
    const smaller = { width: 40, height: 30 }

    expect(clampPosition({ x: 60, y: 60 }, tiny, smaller)).toEqual({
      x: EDGE_MARGIN_PX,
      y: EDGE_MARGIN_PX,
    })
  })

  it('honours a custom margin', () => {
    expect(clampPosition({ x: 0, y: 0 }, element, container, 20)).toEqual({ x: 20, y: 20 })
  })
})
