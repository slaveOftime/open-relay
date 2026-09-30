import { describe, expect, it } from 'vitest'
import {
  DEFAULT_QUICK_KEYS,
  describeData,
  encodeCombo,
  formatCombo,
  moveQuickKey,
  parseCombo,
} from './quick-keys'
import { loadQuickKeys, normalizeQuickKeys, saveQuickKeys } from '@/lib/quickKeysStorage'

function fakeStorage(initial: Record<string, string> = {}) {
  const data = new Map(Object.entries(initial))
  return {
    getItem: (key: string) => data.get(key) ?? null,
    setItem: (key: string, value: string) => void data.set(key, value),
    data,
  }
}

describe('encodeCombo', () => {
  it('encodes control combinations', () => {
    expect(encodeCombo('ctrl+c')).toBe('\x03')
    expect(encodeCombo('Ctrl+P')).toBe('\x10')
    expect(encodeCombo('ctrl+space')).toBe('\x00')
    expect(encodeCombo('ctrl+[')).toBe('\x1b')
  })

  it('encodes named keys and shift+tab', () => {
    expect(encodeCombo('up')).toBe('\x1b[A')
    expect(encodeCombo('enter')).toBe('\r')
    expect(encodeCombo('shift+tab')).toBe('\x1b[Z')
    expect(encodeCombo('tab')).toBe('\t')
  })

  it('encodes literal characters and alt-prefixed keys', () => {
    expect(encodeCombo('shift+!')).toBe('!')
    expect(encodeCombo('!')).toBe('!')
    expect(encodeCombo('alt+f')).toBe('\x1bf')
  })

  it('rejects unknown combos', () => {
    expect(encodeCombo('ctrl+plus')).toBeNull()
    expect(encodeCombo('hyper+x')).toBeNull()
    expect(encodeCombo('ctrl+')).toBeNull()
    expect(encodeCombo('')).toBeNull()
  })
})

describe('parseCombo', () => {
  it('treats a trailing plus as the literal plus key', () => {
    expect(parseCombo('ctrl++')).toEqual({ ctrl: true, shift: false, alt: false, key: '+' })
  })
})

describe('formatCombo', () => {
  it('renders human readable combos', () => {
    expect(formatCombo('ctrl+c')).toBe('Ctrl+C')
    expect(formatCombo('shift+tab')).toBe('Shift+Tab')
    expect(formatCombo('up')).toBe('↑')
    expect(formatCombo('!')).toBe('!'
    )
  })
})

describe('describeData', () => {
  it('renders control bytes compactly', () => {
    expect(describeData('\x03')).toBe('^C')
    expect(describeData('\x1b[Z')).toBe('␛[Z')
    expect(describeData('!')).toBe('!')
  })
})

describe('moveQuickKey', () => {
  it('moves an entry to another index', () => {
    const moved = moveQuickKey(DEFAULT_QUICK_KEYS, 0, 2)
    expect(moved.map((key) => key.id)).toEqual([
      DEFAULT_QUICK_KEYS[1].id,
      DEFAULT_QUICK_KEYS[2].id,
      DEFAULT_QUICK_KEYS[0].id,
      ...DEFAULT_QUICK_KEYS.slice(3).map((key) => key.id),
    ])
  })

  it('returns the same list for no-op moves', () => {
    expect(moveQuickKey(DEFAULT_QUICK_KEYS, 1, 1)).toBe(DEFAULT_QUICK_KEYS)
    expect(moveQuickKey(DEFAULT_QUICK_KEYS, 0, 99)).toBe(DEFAULT_QUICK_KEYS)
  })
})

describe('quick keys storage', () => {
  it('falls back to defaults when nothing is stored', () => {
    expect(loadQuickKeys(fakeStorage())).toEqual(DEFAULT_QUICK_KEYS)
  })

  it('round-trips through a storage port', () => {
    const port = fakeStorage()
    const custom = [DEFAULT_QUICK_KEYS[1], DEFAULT_QUICK_KEYS[0]]
    saveQuickKeys(custom, port)
    expect(loadQuickKeys(port)).toEqual(custom)
  })

  it('falls back to defaults on corrupt data', () => {
    const port = fakeStorage({ 'oly.terminal.quickKeys.v1': 'not json' })
    expect(loadQuickKeys(port)).toEqual(DEFAULT_QUICK_KEYS)
  })

  it('rejects malformed entries', () => {
    expect(normalizeQuickKeys([{ id: 'a', label: 'A', data: '\x03' }])).toBeNull()
    expect(normalizeQuickKeys([{ id: 'a', label: '', data: '\x03', combo: 'ctrl+c' }])).toBeNull()
    expect(normalizeQuickKeys('nope')).toBeNull()
  })
})
