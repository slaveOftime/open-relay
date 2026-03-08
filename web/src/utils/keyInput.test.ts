import { describe, it, expect } from 'vitest'
import { namedKeySequence, parseKeySpec, parseKeyInputSpecs, modifierToken } from './keyInput'

// ── namedKeySequence ──────────────────────────────────────────────────────────

describe('namedKeySequence', () => {
  it('enter variants', () => {
    expect(namedKeySequence('enter')).toBe('\r')
    expect(namedKeySequence('return')).toBe('\r')
    expect(namedKeySequence('cr')).toBe('\r')
  })

  it('lf / linefeed', () => {
    expect(namedKeySequence('lf')).toBe('\n')
    expect(namedKeySequence('linefeed')).toBe('\n')
  })

  it('tab', () => {
    expect(namedKeySequence('tab')).toBe('\t')
  })

  it('backspace and bs alias', () => {
    expect(namedKeySequence('backspace')).toBe('\x08')
    expect(namedKeySequence('bs')).toBe('\x08')
  })

  it('esc / escape', () => {
    expect(namedKeySequence('esc')).toBe('\x1b')
    expect(namedKeySequence('escape')).toBe('\x1b')
  })

  it('arrow keys', () => {
    expect(namedKeySequence('up')).toBe('\x1b[A')
    expect(namedKeySequence('down')).toBe('\x1b[B')
    expect(namedKeySequence('right')).toBe('\x1b[C')
    expect(namedKeySequence('left')).toBe('\x1b[D')
  })

  it('navigation keys', () => {
    expect(namedKeySequence('home')).toBe('\x1b[H')
    expect(namedKeySequence('end')).toBe('\x1b[F')
    expect(namedKeySequence('delete')).toBe('\x1b[3~')
    expect(namedKeySequence('del')).toBe('\x1b[3~')
    expect(namedKeySequence('insert')).toBe('\x1b[2~')
    expect(namedKeySequence('ins')).toBe('\x1b[2~')
    expect(namedKeySequence('pageup')).toBe('\x1b[5~')
    expect(namedKeySequence('pgup')).toBe('\x1b[5~')
    expect(namedKeySequence('pagedown')).toBe('\x1b[6~')
    expect(namedKeySequence('pgdn')).toBe('\x1b[6~')
  })

  it('unknown returns null', () => {
    expect(namedKeySequence('foobar')).toBeNull()
    expect(namedKeySequence('')).toBeNull()
  })
})

// ── parseKeySpec – ctrl ───────────────────────────────────────────────────────

describe('parseKeySpec – ctrl', () => {
  it('ctrl+c is ASCII 3', () => {
    expect(parseKeySpec('ctrl+c').charCodeAt(0)).toBe(3)
  })

  it('ctrl-a (dash separator)', () => {
    expect(parseKeySpec('ctrl-a').charCodeAt(0)).toBe(1)
  })

  it('uppercase CTRL+C treated as lowercase', () => {
    expect(parseKeySpec('CTRL+C').charCodeAt(0)).toBe(3)
  })

  it('full alphabet ctrl+a through ctrl+z maps to ASCII 1–26', () => {
    const letters = 'abcdefghijklmnopqrstuvwxyz'
    for (let i = 0; i < letters.length; i++) {
      const result = parseKeySpec(`ctrl+${letters[i]}`)
      expect(result.charCodeAt(0)).toBe(i + 1)
    }
  })

  it('multi-char ctrl target is unsupported (error)', () => {
    expect(() => parseKeySpec('ctrl+ab')).toThrow()
  })
})

// ── parseKeySpec – plain char / hex ──────────────────────────────────────────

describe('parseKeySpec – plain char', () => {
  it('single lowercase char is passed through', () => {
    expect(parseKeySpec('c')).toBe('c')
  })

  it('single uppercase char is passed through', () => {
    expect(parseKeySpec('Z')).toBe('Z')
  })
})

describe('parseKeySpec – hex notation', () => {
  it('0x prefix ESC', () => {
    expect(parseKeySpec('0x1b')).toBe('\x1b')
  })

  it('backslash-x prefix ESC', () => {
    expect(parseKeySpec('\\x1b')).toBe('\x1b')
  })

  it('backslash-x prefix NUL', () => {
    expect(parseKeySpec('\\x03')).toBe('\x03')
  })

  it('multi-byte hex 0x1b5b produces ESC[', () => {
    expect(parseKeySpec('0x1b5b')).toBe('\x1b[')
  })

  it('odd-length hex is an error', () => {
    expect(() => parseKeySpec('0x1')).toThrow()
  })
})

// ── parseKeySpec – shift ──────────────────────────────────────────────────────

describe('parseKeySpec – shift', () => {
  it('shift+letter uppercases', () => {
    expect(parseKeySpec('shift+a')).toBe('A')
    expect(parseKeySpec('shift+z')).toBe('Z')
  })

  it('shift+tab produces backtab sequence', () => {
    expect(parseKeySpec('shift+tab')).toBe('\x1b[Z')
    expect(parseKeySpec('shift-tab')).toBe('\x1b[Z')
  })

  it('shift+digit produces symbol', () => {
    expect(parseKeySpec('shift+1')).toBe('!')
    expect(parseKeySpec('shift+2')).toBe('@')
  })

  it('shift number row complete', () => {
    expect(parseKeySpec('shift+3')).toBe('#')
    expect(parseKeySpec('shift+4')).toBe('$')
    expect(parseKeySpec('shift+5')).toBe('%')
    expect(parseKeySpec('shift+6')).toBe('^')
    expect(parseKeySpec('shift+7')).toBe('&')
    expect(parseKeySpec('shift+8')).toBe('*')
    expect(parseKeySpec('shift+9')).toBe('(')
    expect(parseKeySpec('shift+0')).toBe(')')
  })

  it('shift punctuation', () => {
    expect(parseKeySpec('shift+-')).toBe('_')
    expect(parseKeySpec('shift+=')).toBe('+')
    expect(parseKeySpec('shift+;')).toBe(':')
    expect(parseKeySpec("shift+'")).toBe('"')
    expect(parseKeySpec('shift+,')).toBe('<')
    expect(parseKeySpec('shift+.')).toBe('>')
    expect(parseKeySpec('shift+/')).toBe('?')
    expect(parseKeySpec('shift+`')).toBe('~')
    expect(parseKeySpec('shift+[')).toBe('{')
    expect(parseKeySpec('shift+]')).toBe('}')
    expect(parseKeySpec('shift+\\')).toBe('|')
  })
})

// ── parseKeySpec – alt / meta ─────────────────────────────────────────────────

describe('parseKeySpec – alt / meta', () => {
  it('alt+letter prepends ESC', () => {
    expect(parseKeySpec('alt+x')).toBe('\x1bx')
  })

  it('meta+letter same as alt', () => {
    expect(parseKeySpec('meta+x')).toBe('\x1bx')
  })

  it('alt+named-key prepends ESC before sequence', () => {
    expect(parseKeySpec('alt+up')).toBe('\x1b\x1b[A')
  })

  it('alt arrow keys', () => {
    expect(parseKeySpec('alt+left')).toBe('\x1b\x1b[D')
    expect(parseKeySpec('alt+right')).toBe('\x1b\x1b[C')
    expect(parseKeySpec('alt+down')).toBe('\x1b\x1b[B')
  })

  it('alt+home and alt+end', () => {
    expect(parseKeySpec('alt+home')).toBe('\x1b\x1b[H')
    expect(parseKeySpec('alt+end')).toBe('\x1b\x1b[F')
  })

  it('alt+ctrl+c produces ESC + control char', () => {
    const result = parseKeySpec('alt+ctrl+c')
    expect(result.charCodeAt(0)).toBe(0x1b)
    expect(result.charCodeAt(1)).toBe(0x03)
  })
})

// ── parseKeySpec – capslock ───────────────────────────────────────────────────

describe('parseKeySpec – capslock', () => {
  it('caps+letter uppercases', () => {
    expect(parseKeySpec('caps+a')).toBe('A')
    expect(parseKeySpec('capslock+b')).toBe('B')
  })

  it('caps+non-alpha is unchanged', () => {
    expect(parseKeySpec('caps+1')).toBe('1')
  })

  it('caps+digit passthrough', () => {
    expect(parseKeySpec('capslock+5')).toBe('5')
  })
})

// ── parseKeySpec – named keys via parseKeySpec ────────────────────────────────

describe('parseKeySpec – named keys', () => {
  it('enter', () => {
    expect(parseKeySpec('enter')).toBe('\r')
  })

  it('ESC (uppercased input)', () => {
    expect(parseKeySpec('ESC')).toBe('\x1b')
  })

  it('tab', () => {
    expect(parseKeySpec('tab')).toBe('\t')
  })
})

// ── parseKeySpec – errors ─────────────────────────────────────────────────────

describe('parseKeySpec – error paths', () => {
  it('empty string is an error', () => {
    expect(() => parseKeySpec('')).toThrow()
    expect(() => parseKeySpec('   ')).toThrow()
  })

  it('modifier-only is an error', () => {
    expect(() => parseKeySpec('ctrl')).toThrow()
    expect(() => parseKeySpec('shift')).toThrow()
    expect(() => parseKeySpec('alt')).toThrow()
    expect(() => parseKeySpec('meta')).toThrow()
    expect(() => parseKeySpec('caps')).toThrow()
    expect(() => parseKeySpec('capslock')).toThrow()
  })

  it('unsupported key is an error', () => {
    expect(() => parseKeySpec('f1')).toThrow()
  })
})

// ── parseKeyInputSpecs ────────────────────────────────────────────────────────

describe('parseKeyInputSpecs', () => {
  it('ctrl then char (separate tokens) combines to ctrl+char', () => {
    const parsed = parseKeyInputSpecs(['ctrl', 'c'])
    expect(parsed.length).toBe(1)
    expect(parsed[0].charCodeAt(0)).toBe(3)
  })

  it('shift then tab (separate tokens) combines to shift+tab = backtab', () => {
    const parsed = parseKeyInputSpecs(['shift', 'tab'])
    expect(parsed).toEqual(['\x1b[Z'])
  })

  it('alt then named key combines correctly', () => {
    expect(parseKeyInputSpecs(['alt', 'up'])).toEqual(['\x1b\x1b[A'])
  })

  it('multiple keys preserve order', () => {
    const parsed = parseKeyInputSpecs(['up', 'enter', 'tab'])
    expect(parsed).toEqual(['\x1b[A', '\r', '\t'])
  })

  it('empty slice returns empty array', () => {
    expect(parseKeyInputSpecs([])).toEqual([])
  })

  it('empty string token is an error', () => {
    expect(() => parseKeyInputSpecs([''])).toThrow()
  })

  it('trailing modifier (no following key) is an error', () => {
    expect(() => parseKeyInputSpecs(['ctrl'])).toThrow()
  })

  it('consecutive modifiers is an error', () => {
    expect(() => parseKeyInputSpecs(['ctrl', 'alt', 'c'])).toThrow()
  })
})

// ── modifierToken ─────────────────────────────────────────────────────────────

describe('modifierToken', () => {
  it('recognises all modifier names', () => {
    expect(modifierToken('ctrl')).toBe('ctrl')
    expect(modifierToken('control')).toBe('ctrl')
    expect(modifierToken('alt')).toBe('alt')
    expect(modifierToken('meta')).toBe('meta')
    expect(modifierToken('shift')).toBe('shift')
    expect(modifierToken('caps')).toBe('capslock')
    expect(modifierToken('capslock')).toBe('capslock')
  })

  it('non-modifier returns null', () => {
    expect(modifierToken('enter')).toBeNull()
    expect(modifierToken('a')).toBeNull()
    expect(modifierToken('')).toBeNull()
  })
})
