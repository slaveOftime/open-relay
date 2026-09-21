// Protocol evidence (post-review corrective increment, W4): decode every
// frame in the shared fixture tests/fixtures/ws_frames.json and assert the
// decoded fields match the fixture's `expect` description exactly. The Rust
// encoder test (src/http/ws.rs::ws_frame_fixture_matches_the_encoder) proves
// the server emits exactly these bytes for that description — so the web
// client and the daemon can never drift apart without a test failing.
import { describe, expect, it } from 'vitest'
import fixture from '../../../tests/fixtures/ws_frames.json'
import {
  parseServerFrame,
  terminalModeSequences,
  type ServerFrame,
  type WsModes,
} from './ws-frames.ts'

interface FrameVector {
  name: string
  hex: string
  expect: Record<string, unknown> & { type: string }
}

function loadVectors(): FrameVector[] {
  return fixture.frames
}

function fromHex(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2)
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16)
  }
  return bytes
}

function toHex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('')
}

/** Project a decoded frame onto the fixture's `expect` shape for comparison. */
function describeFrame(frame: ServerFrame): Record<string, unknown> {
  switch (frame.type) {
    case 'init':
      return {
        type: 'init',
        app_cursor_keys: frame.appCursorKeys,
        bracketed_paste_mode: frame.bracketedPasteMode,
        mouse_report: frame.mouseReport,
        sgr_mouse: frame.sgrMouse,
        focus_events: frame.focusEvents,
        end_offset: frame.endOffset,
        incarnation: frame.incarnation,
        running: frame.running,
        attachment_id: frame.attachmentId,
        role: frame.role,
        data_hex: toHex(frame.data),
      }
    case 'data':
      return { type: 'data', offset: frame.offset, data_hex: toHex(frame.data) }
    case 'modeChanged':
      return {
        type: 'mode_changed',
        app_cursor_keys: frame.appCursorKeys,
        bracketed_paste_mode: frame.bracketedPasteMode,
        mouse_report: frame.mouseReport,
        sgr_mouse: frame.sgrMouse,
        focus_events: frame.focusEvents,
      }
    case 'resized':
      return { type: 'resized', rows: frame.rows, cols: frame.cols }
    case 'sessionEnded':
      return { type: 'session_ended', exit_code: frame.exitCode, final_offset: frame.finalOffset }
    case 'error':
      return { type: 'error', message: frame.message }
    case 'control':
      return { type: 'control', role: frame.role }
    case 'pong':
      return { type: 'pong' }
  }
}

describe('ws frame fixture conformance', () => {
  const vectors = loadVectors()

  it('covers every server frame kind', () => {
    expect(vectors.length).toBeGreaterThanOrEqual(10)
    const kinds = new Set(vectors.map((v) => v.expect.type))
    for (const kind of [
      'init',
      'data',
      'mode_changed',
      'resized',
      'session_ended',
      'error',
      'control',
      'pong',
    ]) {
      expect(kinds.has(kind), `fixture covers ${kind}`).toBe(true)
    }
  })

  for (const vector of loadVectors()) {
    it(`decodes ${vector.name} exactly as described`, () => {
      const frame = parseServerFrame(fromHex(vector.hex))
      expect(frame, `${vector.name} must decode`).not.toBeNull()
      expect(describeFrame(frame!)).toEqual(vector.expect)
    })
  }

  it('mirrors the negotiated modes as an authoritative DECSET stream', () => {
    const modes: WsModes = {
      appCursorKeys: false,
      bracketedPasteMode: true,
      mouseReport: true,
      sgrMouse: true,
      focusEvents: false,
    }
    const seq = terminalModeSequences(modes)
    // Set-or-clear for every mode: stale replay bytes can never leave a
    // wrong capture state behind.
    expect(seq).toBe(
      '\x1b[?1l\x1b[?2004h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1000h\x1b[?1006h\x1b[?1004l'
    )
    expect(seq).toContain('\x1b[?1000h')
    expect(seq).toContain('\x1b[?1006h')
    expect(terminalModeSequences({ ...modes, mouseReport: false, sgrMouse: false })).not.toContain(
      '\x1b[?1000h'
    )
  })

  it('fails loudly on truncated and unknown frames instead of misparsing them', () => {
    // Empty payload is the only ignorable result ("no frame").
    expect(parseServerFrame(new Uint8Array(0))).toBeNull()
    // Unknown tag: throwing beats silently dropping stream bytes (I2).
    expect(() => parseServerFrame(new Uint8Array([255, 1, 2]))).toThrow(/unknown server frame tag/)
    // INIT header is 28 bytes; 27 must not decode.
    expect(() => parseServerFrame(fromHex(vectors[0].hex).subarray(0, 27))).toThrow(
      /truncated init frame/
    )
    // DATA header is 9 bytes; 8 must not decode.
    expect(() =>
      parseServerFrame(fromHex(vectors.find((v) => v.name === 'data')!.hex).subarray(0, 8))
    ).toThrow(/truncated data frame/)
  })
})
