import { afterEach, describe, expect, it, vi } from 'vitest'

import { DEFAULT_QUICK_KEYS, type QuickKey } from '@/lib/quick-keys'

const {
  getQuickKeys,
  loadQuickKeys,
  normalizeQuickKeys,
  resetQuickKeys,
  saveQuickKeys,
  setQuickKeys,
  subscribeQuickKeys,
} = await import('./quick-keys-storage')

function quickKey(overrides: Partial<QuickKey> = {}): QuickKey {
  return {
    id: 'k1',
    label: '^C',
    data: '\x03',
    combo: 'ctrl+c',
    ...overrides,
  }
}

/** A storage stand-in; the module takes a port, so no global stubbing is needed. */
function storage(initial: Record<string, string> = {}) {
  const data = new Map(Object.entries(initial))
  return {
    getItem: (key: string) => (data.has(key) ? data.get(key)! : null),
    setItem: (key: string, value: string) => void data.set(key, value),
    data,
  }
}

/** Same shape, but every operation throws — private mode, or PWA quota. */
function brokenStorage() {
  return {
    getItem: () => {
      throw new Error('unavailable')
    },
    setItem: () => {
      throw new Error('quota')
    },
  }
}

const KEY = 'oly.terminal.quickKeys.v1'

afterEach(() => {
  vi.unstubAllGlobals()
  resetQuickKeys()
})

describe('normalizeQuickKeys', () => {
  it('keeps a valid list and drops nothing', () => {
    const keys = [quickKey(), quickKey({ id: 'k2', label: '!', data: '!', combo: 'shift+!' })]

    expect(normalizeQuickKeys(keys)).toEqual(keys)
  })

  it('returns null for anything that is not an array', () => {
    expect(normalizeQuickKeys(null)).toBeNull()
    expect(normalizeQuickKeys(undefined)).toBeNull()
    expect(normalizeQuickKeys({})).toBeNull()
    expect(normalizeQuickKeys('[]')).toBeNull()
  })

  it('rejects the whole list when one entry is malformed', () => {
    // A half-valid list would silently drop the user's keys one by one; the
    // contract is all-or-nothing so the default set replaces it instead.
    for (const bad of [
      null,
      42,
      'text',
      {},
      quickKey({ id: '' }),
      quickKey({ label: '' }),
      quickKey({ data: '' }),
      quickKey({ combo: 42 as unknown as string }),
      quickKey({ color: 'not-a-color' }),
      quickKey({ color: '#ff00' }),
    ]) {
      expect(normalizeQuickKeys([quickKey(), bad])).toBeNull()
    }
  })

  it('accepts a valid hex color and omits an unset one', () => {
    expect(normalizeQuickKeys([quickKey({ color: '#ef4444' })])).toEqual([
      quickKey({ color: '#ef4444' }),
    ])
    const noColor = normalizeQuickKeys([quickKey()])![0]

    expect('color' in noColor).toBe(false)
  })

  it('rejects duplicate ids', () => {
    // Two keys sharing an id would both react to one editor edit.
    expect(normalizeQuickKeys([quickKey(), quickKey()])).toBeNull()
  })
})

describe('loadQuickKeys', () => {
  it('returns the defaults with nothing stored', () => {
    expect(loadQuickKeys(storage())).toEqual(DEFAULT_QUICK_KEYS)
  })

  it('round-trips what was saved', () => {
    const port = storage()
    const keys = [quickKey({ color: '#ef4444' })]
    saveQuickKeys(keys, port)

    expect(loadQuickKeys(port)).toEqual(keys)
  })

  it('falls back to the defaults for corrupt JSON', () => {
    expect(loadQuickKeys(storage({ [KEY]: '<nope>' }))).toEqual(DEFAULT_QUICK_KEYS)
  })

  it('falls back to the defaults for a shape that fails validation', () => {
    expect(loadQuickKeys(storage({ [KEY]: JSON.stringify([{ id: '' }]) }))).toEqual(
      DEFAULT_QUICK_KEYS
    )
  })

  it('falls back to the defaults when storage throws', () => {
    expect(loadQuickKeys(brokenStorage())).toEqual(DEFAULT_QUICK_KEYS)
  })

  it('falls back to the defaults with no storage at all', () => {
    expect(loadQuickKeys(undefined)).toEqual(DEFAULT_QUICK_KEYS)
  })
})

describe('the observable store', () => {
  it('notifies subscribers when the keys change', () => {
    const listener = vi.fn()
    const unsubscribe = subscribeQuickKeys(listener)

    setQuickKeys([quickKey()])

    expect(listener).toHaveBeenCalledOnce()

    unsubscribe()
    setQuickKeys([quickKey({ id: 'k2' })])

    expect(listener).toHaveBeenCalledOnce()
  })

  it('serves the same list until it is replaced', () => {
    const keys = [quickKey()]
    setQuickKeys(keys)

    expect(getQuickKeys()).toBe(keys)
    expect(getQuickKeys()).toBe(keys)
  })

  it('reset restores the defaults and persists them', () => {
    const port = storage()
    setQuickKeys([quickKey()])
    saveQuickKeys([quickKey()], port)

    resetQuickKeys()

    expect(getQuickKeys()).toEqual(DEFAULT_QUICK_KEYS)
  })
})
