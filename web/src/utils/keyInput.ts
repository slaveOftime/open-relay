const SHIFT_SYMBOL_MAP: Record<string, string> = {
  '1': '!',
  '2': '@',
  '3': '#',
  '4': '$',
  '5': '%',
  '6': '^',
  '7': '&',
  '8': '*',
  '9': '(',
  '0': ')',
  '-': '_',
  '=': '+',
  '[': '{',
  ']': '}',
  '\\': '|',
  ';': ':',
  "'": '"',
  ',': '<',
  '.': '>',
  '/': '?',
  '`': '~',
}

function singleChar(value: string): string | null {
  return Array.from(value).length === 1 ? value : null
}

function modifierPayload(raw: string, normalized: string, prefixes: string[]): string | null {
  for (const prefix of prefixes) {
    if (normalized.startsWith(prefix)) {
      const payload = raw.slice(prefix.length).trim()
      if (payload.length > 0) return payload
    }
  }
  return null
}

function parseHexSequence(raw: string): string | null {
  let payload: string
  if (
    raw.startsWith('0x') ||
    raw.startsWith('0X') ||
    raw.startsWith('\\x') ||
    raw.startsWith('\\X')
  ) {
    payload = raw.slice(2)
  } else {
    return null
  }

  if (payload.length === 0 || payload.length % 2 !== 0) return null
  if (!/^[0-9a-fA-F]+$/.test(payload)) return null

  const bytes: number[] = []
  for (let index = 0; index < payload.length; index += 2) {
    const pair = payload.slice(index, index + 2)
    bytes.push(Number.parseInt(pair, 16))
  }
  return new TextDecoder().decode(new Uint8Array(bytes))
}

function parseCtrlKey(normalized: string): string | null {
  const payload = normalized.startsWith('ctrl+')
    ? normalized.slice(5)
    : normalized.startsWith('ctrl-')
      ? normalized.slice(5)
      : null

  if (!payload) return null
  if (Array.from(payload).length !== 1) return null

  const code = payload.charCodeAt(0)
  if (code > 0x7f) return null
  const lower = payload.toLowerCase().charCodeAt(0)
  return String.fromCharCode(lower & 0x1f)
}

function parseShiftKey(raw: string, normalized: string): string | null {
  const payload = modifierPayload(raw, normalized, ['shift+', 'shift-'])
  if (!payload) return null

  if (payload.toLowerCase() === 'tab') return '\x1b[Z'

  const ch = singleChar(payload)
  if (!ch) return null

  if (/^[a-z]$/.test(ch)) return ch.toUpperCase()
  return SHIFT_SYMBOL_MAP[ch] ?? ch
}

function parseCapsKey(raw: string, normalized: string): string | null {
  const payload = modifierPayload(raw, normalized, ['caps+', 'caps-', 'capslock+', 'capslock-'])
  if (!payload) return null

  const ch = singleChar(payload)
  if (!ch) return null

  return /^[a-zA-Z]$/.test(ch) ? ch.toUpperCase() : ch
}

function parseAltKey(raw: string, normalized: string): string | null {
  const payload = modifierPayload(raw, normalized, ['alt+', 'alt-', 'meta+', 'meta-'])
  if (!payload) return null

  const payloadNormalized = payload.toLowerCase()
  const named = namedKeySequence(payloadNormalized)
  if (named) return `\x1b${named}`

  const ctrl = parseCtrlKey(payloadNormalized)
  if (ctrl) return `\x1b${ctrl}`

  const ch = singleChar(payload)
  if (!ch) return null

  return `\x1b${ch}`
}

export function modifierToken(normalized: string): string | null {
  switch (normalized) {
    case 'ctrl':
    case 'control':
      return 'ctrl'
    case 'alt':
      return 'alt'
    case 'meta':
      return 'meta'
    case 'shift':
      return 'shift'
    case 'caps':
    case 'capslock':
      return 'capslock'
    default:
      return null
  }
}

export function namedKeySequence(normalized: string): string | null {
  switch (normalized) {
    case 'enter':
    case 'return':
    case 'cr':
      return '\r'
    case 'lf':
    case 'linefeed':
      return '\n'
    case 'tab':
      return '\t'
    case 'backspace':
    case 'bs':
      return '\x08'
    case 'esc':
    case 'escape':
      return '\x1b'
    case 'up':
      return '\x1b[A'
    case 'down':
      return '\x1b[B'
    case 'right':
      return '\x1b[C'
    case 'left':
      return '\x1b[D'
    case 'home':
      return '\x1b[H'
    case 'end':
      return '\x1b[F'
    case 'delete':
    case 'del':
      return '\x1b[3~'
    case 'insert':
    case 'ins':
      return '\x1b[2~'
    case 'pageup':
    case 'pgup':
      return '\x1b[5~'
    case 'pagedown':
    case 'pgdn':
      return '\x1b[6~'
    default:
      return null
  }
}

export function parseKeySpec(spec: string): string {
  const trimmed = spec.trim()
  const normalized = trimmed.toLowerCase()

  if (!normalized) {
    throw new Error('empty --key value is not allowed')
  }

  const named = namedKeySequence(normalized)
  if (named) return named

  const hex = parseHexSequence(trimmed)
  if (hex !== null) return hex

  const ch = singleChar(trimmed)
  if (ch) return ch

  const ctrl = parseCtrlKey(normalized)
  if (ctrl) return ctrl

  const shift = parseShiftKey(trimmed, normalized)
  if (shift !== null) return shift

  const caps = parseCapsKey(trimmed, normalized)
  if (caps !== null) return caps

  const alt = parseAltKey(trimmed, normalized)
  if (alt !== null) return alt

  if (['shift', 'alt', 'meta', 'ctrl', 'caps', 'capslock'].includes(normalized)) {
    throw new Error(
      'modifier-only --key is not supported; use forms like shift+tab, alt+x, ctrl+c, capslock+a'
    )
  }

  throw new Error(
    `unsupported --key \`${spec}\`; use named keys (enter/esc/tab/up/down/left/right/home/end/pgup/pgdn/del/ins), ctrl+<char>, alt+<char|named-key>, shift+<char|tab>, or capslock+<letter>`
  )
}

export function parseKeyInputSpecs(specs: string[]): string[] {
  const parsed: string[] = []
  let pendingModifier: string | null = null

  for (const spec of specs) {
    const trimmed = spec.trim()
    const normalized = trimmed.toLowerCase()

    if (!normalized) {
      throw new Error('empty --key value is not allowed')
    }

    const modifier = modifierToken(normalized)
    if (modifier) {
      if (pendingModifier) {
        throw new Error(
          `modifier --key \`${pendingModifier}\` must be followed by a key value before \`${modifier}\``
        )
      }
      pendingModifier = modifier
      continue
    }

    const effective = pendingModifier ? `${pendingModifier}+${trimmed}` : trimmed
    pendingModifier = null
    parsed.push(parseKeySpec(effective))
  }

  if (pendingModifier) {
    throw new Error(`modifier --key \`${pendingModifier}\` must be followed by a key value`)
  }

  return parsed
}

export function splitKeyInput(input: string): string[] {
  return input.trim().split(/\s+/).filter(Boolean)
}
