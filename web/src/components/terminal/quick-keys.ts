/**
 * Quick-keys model: pure, side-effect-free helpers shared by the radial
 * menu, the editor dialog and the localStorage layer.
 *
 * A combo string ("ctrl+c", "shift+tab", "alt+f", "!") is encoded into the
 * byte sequence a terminal expects on the wire. Combos that cannot be
 * encoded fall back to being sent as literal text.
 */

export interface QuickKey {
  id: string
  /** Short text shown on the radial button, e.g. "^C". */
  label: string
  /** Byte sequence written to the PTY when the key is tapped. */
  data: string
  /** Combo the key was created from, shown in the editor ("ctrl+c"). */
  combo: string
}

const MODIFIER_ALIASES: Record<string, 'ctrl' | 'shift' | 'alt'> = {
  ctrl: 'ctrl',
  control: 'ctrl',
  shift: 'shift',
  alt: 'alt',
  opt: 'alt',
  option: 'alt',
}

const NAMED_KEYS: Record<string, string> = {
  tab: '\t',
  enter: '\r',
  return: '\r',
  esc: '\x1b',
  escape: '\x1b',
  space: ' ',
  backspace: '\x7f',
  delete: '\x1b[3~',
  del: '\x1b[3~',
  insert: '\x1b[2~',
  home: '\x1b[H',
  end: '\x1b[F',
  up: '\x1b[A',
  down: '\x1b[B',
  right: '\x1b[C',
  left: '\x1b[D',
  pageup: '\x1b[5~',
  pgup: '\x1b[5~',
  pagedown: '\x1b[6~',
  pgdn: '\x1b[6~',
  f1: '\x1bOP',
  f2: '\x1bOQ',
  f3: '\x1bOR',
  f4: '\x1bOS',
}

const NAMED_DISPLAY: Record<string, string> = {
  tab: 'Tab',
  enter: 'Enter',
  return: 'Enter',
  esc: 'Esc',
  escape: 'Esc',
  space: 'Space',
  backspace: 'Bksp',
  delete: 'Del',
  del: 'Del',
  insert: 'Ins',
  home: 'Home',
  end: 'End',
  up: '↑',
  down: '↓',
  right: '→',
  left: '←',
  pageup: 'PgUp',
  pgup: 'PgUp',
  pagedown: 'PgDn',
  pgdn: 'PgDn',
}

export interface ParsedCombo {
  ctrl: boolean
  shift: boolean
  alt: boolean
  key: string
}

export function parseCombo(raw: string): ParsedCombo | null {
  let parts = raw.trim().toLowerCase().split('+')
  if (parts.length === 0 || parts.every((part) => part === '')) return null
  // "ctrl++" splits into ["ctrl", "", ""]: the doubled empty parts mean the
  // literal "+" key. A single trailing empty ("ctrl+") is a typo.
  if (parts.length > 2 && parts[parts.length - 1] === '' && parts[parts.length - 2] === '') {
    parts = [...parts.slice(0, -2), '+']
  }
  const key = parts[parts.length - 1]
  if (!key) return null
  const combo: ParsedCombo = { ctrl: false, shift: false, alt: false, key }
  for (const part of parts.slice(0, -1)) {
    const modifier = MODIFIER_ALIASES[part]
    if (!modifier) return null
    combo[modifier] = true
  }
  return combo
}

/** Encode a combo to its terminal byte sequence, or null if unsupported. */
export function encodeCombo(raw: string): string | null {
  const combo = parseCombo(raw)
  if (!combo) return null
  const { ctrl, shift, alt, key } = combo

  if (ctrl) {
    if (/^[a-z]$/.test(key)) return String.fromCharCode(key.charCodeAt(0) - 96)
    if (key === 'space') return '\x00'
    if (key === '[') return '\x1b'
    return null
  }
  if (shift && key === 'tab') return '\x1b[Z'

  const named = NAMED_KEYS[key]
  if (named !== undefined) return alt ? `\x1b${named}` : named
  if (key.length === 1) return alt ? `\x1b${key}` : key
  return null
}

/** "ctrl+c" -> "Ctrl+C", "shift+tab" -> "Shift+Tab", "!" -> "!" */
export function formatCombo(raw: string): string {
  const combo = parseCombo(raw)
  if (!combo) return raw.trim()
  const { ctrl, shift, alt, key } = combo
  const base = NAMED_DISPLAY[key] ?? (key.length === 1 ? key.toUpperCase() : key)
  return [ctrl && 'Ctrl', shift && 'Shift', alt && 'Alt', base].filter(Boolean).join('+')
}

/** Render a byte sequence compactly: "\x03" -> "^C", "\x1b[Z" -> "␛[Z". */
export function describeData(data: string): string {
  let out = ''
  for (const char of data) {
    const code = char.charCodeAt(0)
    if (char === '\x1b') out += '␛'
    else if (char === '\r') out += '␍'
    else if (code < 32) out += `^${String.fromCharCode(code + 64)}`
    else if (code === 127) out += '^?'
    else out += char
  }
  return out
}

export function moveQuickKey(keys: QuickKey[], from: number, to: number): QuickKey[] {
  if (from === to || from < 0 || to < 0 || from >= keys.length || to >= keys.length) {
    return keys
  }
  const next = [...keys]
  const [moved] = next.splice(from, 1)
  next.splice(to, 0, moved)
  return next
}

/**
 * Radial layout math. Keys fan over a half circle on the free side of the
 * pad (screen-bottom around the left to screen-top; the pad hugs the right
 * edge so the right quarter stays empty). The customize button reserves the
 * bottom slot of the inner ring, keys are placed counter-clockwise starting
 * next to it, and any keys that no longer fit at a tappable spacing spill
 * onto further concentric rings.
 */
export const BASE_RING_RADIUS_PX = 72
export const RING_GAP_PX = 60
export const RING_MIN_SPACING_PX = 52

export interface RingPosition {
  dx: number
  dy: number
}

export interface RingLayout {
  /** One position per key, in list order (index 0 sits next to customize). */
  positions: RingPosition[]
  customize: RingPosition
}

export function layoutRing(keysCount: number, maxRadius: number): RingLayout {
  const positions: RingPosition[] = []
  let customize: RingPosition = { dx: 0, dy: BASE_RING_RADIUS_PX }
  let start = 0
  for (let ring = 0; start < keysCount; ring += 1) {
    const naturalRadius = BASE_RING_RADIUS_PX + ring * RING_GAP_PX
    const atLimit = naturalRadius >= maxRadius
    const radius = Math.min(naturalRadius, Math.max(maxRadius, BASE_RING_RADIUS_PX))
    // Even angular spacing keeps centers at least RING_MIN_SPACING_PX apart.
    const capacity = Math.max(Math.floor((Math.PI * radius) / RING_MIN_SPACING_PX), 1) + (ring === 0 ? 0 : 1)
    const remaining = keysCount - start
    const count = atLimit || remaining <= capacity ? remaining : capacity
    for (let local = 0; local < count; local += 1) {
      // Inner ring: slot 0 belongs to customize, keys take slots 1..count
      // so the arc still ends at the top. Outer rings span bottom..top.
      const angle =
        ring === 0
          ? Math.PI / 2 + ((local + 1) / count) * Math.PI
          : count === 1
            ? Math.PI / 2
            : Math.PI / 2 + (local / (count - 1)) * Math.PI
      positions[start + local] = { dx: Math.cos(angle) * radius, dy: Math.sin(angle) * radius }
    }
    if (ring === 0) customize = { dx: 0, dy: radius }
    start += count
  }
  return { positions, customize }
}

function quickKey(combo: string, label: string): QuickKey {
  const data = encodeCombo(combo)
  if (data === null) throw new Error(`invalid default quick key combo: ${combo}`)
  return { id: combo, label, data, combo }
}

export const DEFAULT_QUICK_KEYS: QuickKey[] = [
  quickKey('ctrl+c', '^C'),
  quickKey('shift+!', '!'),
  quickKey('shift+tab', '⇧⇥'),
  quickKey('ctrl+p', '^P'),
  quickKey('ctrl+a', '^A'),
  quickKey('ctrl+e', '^E'),
  quickKey('ctrl+l', '^L'),
  quickKey('ctrl+d', '^D'),
]
