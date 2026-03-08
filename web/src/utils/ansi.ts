// ---------------------------------------------------------------------------
// ANSI / VT100 escape code parser
// Converts ANSI-escaped text into HTML spans preserving colors and styles.
// ---------------------------------------------------------------------------

interface TextStyle {
  bold: boolean
  dim: boolean
  italic: boolean
  underline: boolean
  strike: boolean
  fg: string | null // CSS color or null
  bg: string | null
  blink: boolean
}

const RESET_STYLE: TextStyle = {
  bold: false,
  dim: false,
  italic: false,
  underline: false,
  strike: false,
  fg: null,
  bg: null,
  blink: false,
}

// Basic 16-color palette (matches standard terminal colors)
const ANSI_COLORS_NORMAL: string[] = [
  '#1a1a1a',
  '#ef4444',
  '#22c55e',
  '#eab308',
  '#3b82f6',
  '#a855f7',
  '#06b6d4',
  '#d1d5db',
]
const ANSI_COLORS_BRIGHT: string[] = [
  '#6b7280',
  '#f87171',
  '#4ade80',
  '#fde047',
  '#60a5fa',
  '#d8b4fe',
  '#67e8f9',
  '#ffffff',
]

function idx256(n: number): string {
  if (n < 8) return ANSI_COLORS_NORMAL[n]
  if (n < 16) return ANSI_COLORS_BRIGHT[n - 8]
  if (n < 232) {
    const v = n - 16
    const b = v % 6,
      g = Math.floor(v / 6) % 6,
      r = Math.floor(v / 36)
    const c = (x: number) =>
      Math.round((x * 255) / 5)
        .toString(16)
        .padStart(2, '0')
    return `#${c(r)}${c(g)}${c(b)}`
  }
  const gray = Math.round(((n - 232) / 23) * 255)
  const h = gray.toString(16).padStart(2, '0')
  return `#${h}${h}${h}`
}

function styleToAttrs(s: TextStyle): string {
  const parts: string[] = []
  if (s.fg) parts.push(`color:${s.fg}`)
  if (s.bg) parts.push(`background:${s.bg}`)
  if (s.bold) parts.push('font-weight:bold')
  if (s.dim) parts.push('opacity:0.6')
  if (s.italic) parts.push('font-style:italic')
  if (s.underline) parts.push('text-decoration:underline')
  if (s.strike) parts.push('text-decoration:line-through')
  if (s.blink) parts.push('animation:cursor-blink 1s step-end infinite')
  return parts.join(';')
}

export function _stylesEqual(a: TextStyle, b: TextStyle): boolean {
  return (
    a.bold === b.bold &&
    a.dim === b.dim &&
    a.italic === b.italic &&
    a.underline === b.underline &&
    a.strike === b.strike &&
    a.fg === b.fg &&
    a.bg === b.bg &&
    a.blink === b.blink
  )
}

function applyCode(style: TextStyle, code: number, _params: number[]): TextStyle {
  const s = { ...style }
  switch (code) {
    case 0:
      return { ...RESET_STYLE }
    case 1:
      s.bold = true
      break
    case 2:
      s.dim = true
      break
    case 3:
      s.italic = true
      break
    case 4:
      s.underline = true
      break
    case 5:
      s.blink = true
      break
    case 9:
      s.strike = true
      break
    case 22:
      s.bold = false
      s.dim = false
      break
    case 23:
      s.italic = false
      break
    case 24:
      s.underline = false
      break
    case 25:
      s.blink = false
      break
    case 29:
      s.strike = false
      break
    case 39:
      s.fg = null
      break
    case 49:
      s.bg = null
      break
    // Normal FG 30-37, bright FG 90-97
    default:
      if (code >= 30 && code <= 37) {
        s.fg = ANSI_COLORS_NORMAL[code - 30]
      } else if (code >= 90 && code <= 97) {
        s.fg = ANSI_COLORS_BRIGHT[code - 90]
      } else if (code >= 40 && code <= 47) {
        s.bg = ANSI_COLORS_NORMAL[code - 40]
      } else if (code >= 100 && code <= 107) {
        s.bg = ANSI_COLORS_BRIGHT[code - 100]
      }
      break
  }
  return s
}

function applySgrParams(style: TextStyle, params: number[]): TextStyle {
  if (params.length === 0) return { ...RESET_STYLE }

  let s = { ...style }
  let i = 0
  while (i < params.length) {
    const p = params[i]
    // 256-color and true-color
    if ((p === 38 || p === 48) && params[i + 1] === 5) {
      const color = idx256(params[i + 2] ?? 0)
      if (p === 38) s.fg = color
      else s.bg = color
      i += 3
      continue
    }
    if ((p === 38 || p === 48) && params[i + 1] === 2) {
      const r = params[i + 2] ?? 0,
        g = params[i + 3] ?? 0,
        b = params[i + 4] ?? 0
      const color = `rgb(${r},${g},${b})`
      if (p === 38) s.fg = color
      else s.bg = color
      i += 5
      continue
    }
    s = applyCode(s, p, params)
    i++
  }
  return s
}

// ---------------------------------------------------------------------------
// Strip ANSI for plain text
// ---------------------------------------------------------------------------

export function stripAnsi(text: string): string {
  return text
    .replace(/\x1b\[[0-9;]*[mGKHJABCDsuSTfhilnprqx]/g, '')
    .replace(/\x1b[()][AB012]/g, '')
    .replace(/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g, '')
}

// ---------------------------------------------------------------------------
// Convert a single line with ANSI codes to HTML
// ---------------------------------------------------------------------------

export function ansiToHtml(line: string): string {
  // Handle carriage return: keep only what's after the last \r
  const lastCr = line.lastIndexOf('\r')
  if (lastCr >= 0) line = line.slice(lastCr + 1)

  // eslint-disable-next-line no-control-regex
  const TOKEN_RE = /\x1b\[([0-9;]*)([mGKHJABCDsuSTfhilnprqx])/g
  let style: TextStyle = { ...RESET_STYLE }
  let html = ''
  let pos = 0

  let match: RegExpExecArray | null
  while ((match = TOKEN_RE.exec(line)) !== null) {
    // Text before this escape sequence
    if (match.index > pos) {
      const segment = escHtml(line.slice(pos, match.index))
      const attrs = styleToAttrs(style)
      html += attrs ? `<span style="${attrs}">${segment}</span>` : segment
    }
    pos = match.index + match[0].length

    const cmd = match[2]
    const params = match[1].split(';').map((n) => (n === '' ? 0 : parseInt(n, 10)))

    if (cmd === 'm') {
      style = applySgrParams(style, params)
    }
    // Other commands (cursor movement, erase) are ignored for line rendering
  }

  // Remaining text
  if (pos < line.length) {
    const segment = escHtml(line.slice(pos))
    const attrs = styleToAttrs(style)
    html += attrs ? `<span style="${attrs}">${segment}</span>` : segment
  }

  return html
}

function escHtml(s: string): string {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
}

// ---------------------------------------------------------------------------
// Render array of ANSI lines into a container element
// ---------------------------------------------------------------------------

export function renderAnsiLines(
  container: HTMLElement,
  lines: string[],
  opts: { autoScroll?: boolean; maxLines?: number } = {}
): void {
  const { autoScroll = true, maxLines = 5000 } = opts

  // Trim if needed
  const toRender = lines.slice(-maxLines)

  const frag = document.createDocumentFragment()
  for (const line of toRender) {
    const div = document.createElement('div')
    div.className = 'whitespace-pre-wrap break-all min-h-[1.4em]'
    div.innerHTML = ansiToHtml(line)
    frag.appendChild(div)
  }
  container.innerHTML = ''
  container.appendChild(frag)

  if (autoScroll) {
    container.scrollTop = container.scrollHeight
  }
}

/** Append new lines to the container (without clearing). */
export function appendAnsiLines(
  container: HTMLElement,
  lines: string[],
  opts: { autoScroll?: boolean; wasAtBottom?: boolean; maxLines?: number } = {}
): void {
  const { autoScroll = true, wasAtBottom = true, maxLines = 5000 } = opts

  for (const line of lines) {
    const div = document.createElement('div')
    div.className = 'whitespace-pre-wrap break-all min-h-[1.4em]'
    div.innerHTML = ansiToHtml(line)
    container.appendChild(div)
  }

  // Trim old lines from top
  while (container.childElementCount > maxLines) {
    container.removeChild(container.firstElementChild!)
  }

  if (autoScroll && wasAtBottom) {
    container.scrollTop = container.scrollHeight
  }
}

/** Returns true if the container is scrolled to (near) the bottom. */
export function isAtBottom(el: HTMLElement, threshold = 40): boolean {
  return el.scrollHeight - el.scrollTop - el.clientHeight < threshold
}
