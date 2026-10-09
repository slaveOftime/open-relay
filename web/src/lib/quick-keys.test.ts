import { describe, expect, it } from 'vitest'
import {
  BASE_RING_RADIUS_PX,
  DEFAULT_QUICK_KEYS,
  QUICK_KEY_COLORS,
  RING_MIN_SPACING_PX,
  describeData,
  encodeCombo,
  encodeQuickKeyCombo,
  formatCombo,
  layoutRing,
  moveQuickKey,
  parseCombo,
} from './quick-keys'
import { loadQuickKeys, normalizeQuickKeys, saveQuickKeys } from '@/lib/quick-keys-storage'

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

describe('encodeQuickKeyCombo', () => {
  it('encodes AttachPanel key specs', () => {
    expect(encodeQuickKeyCombo('ctrl+c')).toBe('\x03')
    expect(encodeQuickKeyCombo('ctrl c')).toBe('ctrlc')
    expect(encodeQuickKeyCombo('enter')).toBe('\r')
    expect(encodeQuickKeyCombo('space')).toBe(' ')
    expect(encodeQuickKeyCombo('ctrl+space')).toBe('\x00')
  })

  it('splits plain words into characters', () => {
    expect(encodeQuickKeyCombo('hello')).toBe('hello')
    expect(encodeQuickKeyCombo('ls -al enter')).toBe('ls-al\r')
    expect(encodeQuickKeyCombo('hello space world')).toBe('hello world')
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
    expect(formatCombo('!')).toBe('!')
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

describe('layoutRing', () => {
  const distance = (a: { dx: number; dy: number }, b: { dx: number; dy: number }) =>
    Math.hypot(a.dx - b.dx, a.dy - b.dy)

  it('reserves the bottom slot for customize and starts keys next to it', () => {
    const layout = layoutRing(4, 400)
    expect(layout.customize).toEqual({ dx: 0, dy: BASE_RING_RADIUS_PX })
    // Keys are start-aligned: they march clockwise (bottom -> left -> top)
    // from the customize slot at a fixed even step and do NOT stretch to
    // fill the half circle.
    const step = RING_MIN_SPACING_PX / BASE_RING_RADIUS_PX
    for (let i = 0; i < 4; i += 1) {
      const angle = Math.PI / 2 + (i + 1) * step
      expect(layout.positions[i].dx).toBeCloseTo(Math.cos(angle) * BASE_RING_RADIUS_PX, 6)
      expect(layout.positions[i].dy).toBeCloseTo(Math.sin(angle) * BASE_RING_RADIUS_PX, 6)
    }
    // With only 4 keys the arc ends well short of the top.
    const last = layout.positions[3]
    expect(Math.hypot(last.dx, last.dy)).toBeCloseTo(BASE_RING_RADIUS_PX, 3)
    expect(Math.atan2(last.dy, last.dx)).toBeLessThan(Math.PI * 1.5 - 0.1)
  })

  it('spills onto a second ring when keys do not fit the first', () => {
    const layout = layoutRing(12, 500)
    expect(layout.positions).toHaveLength(12)
    const inner = layout.positions.filter((p) => Math.hypot(p.dx, p.dy) < 100)
    expect(inner.length).toBeGreaterThan(0)
    expect(inner.length).toBeLessThan(12)
    // Neighboring keys keep a tappable distance everywhere.
    for (let i = 1; i < layout.positions.length; i += 1) {
      expect(distance(layout.positions[i - 1], layout.positions[i])).toBeGreaterThanOrEqual(50)
    }
  })

  it('places every key even when the radius is heavily clamped', () => {
    const layout = layoutRing(10, 80)
    expect(layout.positions).toHaveLength(10)
  })

  it('handles an empty list', () => {
    const layout = layoutRing(0, 400)
    expect(layout.positions).toEqual([])
    expect(layout.customize.dy).toBe(BASE_RING_RADIUS_PX)
  })
})

describe('quick keys storage', () => {
  it('falls back to defaults when nothing is stored', () => {
    expect(loadQuickKeys(fakeStorage())).toEqual(DEFAULT_QUICK_KEYS)
  })

  it('keeps an optional color through the round trip', () => {
    const port = fakeStorage()
    saveQuickKeys([{ ...DEFAULT_QUICK_KEYS[0], color: QUICK_KEY_COLORS[3] }], port)
    expect(loadQuickKeys(port)[0].color).toBe(QUICK_KEY_COLORS[3])
  })

  it('rejects malformed colors', () => {
    expect(
      normalizeQuickKeys([{ id: 'a', label: 'A', data: '\x03', combo: 'ctrl+c', color: 'red' }])
    ).toBeNull()
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
