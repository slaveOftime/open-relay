// Binary attach-frame decoder for the session WebSocket protocol (ADR-0004).
// Pure and exported so the wire format is pinned against the shared fixture
// tests/fixtures/ws_frames.json: the Rust encoder test
// (src/http/ws.rs::ws_frame_fixture_matches_the_encoder) proves the server
// emits exactly these bytes, and ws-frames.test.ts proves this decoder reads
// them back into the same description. The encoder and decoder can never
// drift apart without one side failing.
//
//   INIT:   [tag=1][flags:u8][endOffset:u64be][incarnation:u64be][running:u8]
//           [attachmentId:u64be][role:u8][data]
//   DATA:   [tag=2][offset:u64be][data]
//   ENDED:  [tag=5][hasExitCode:u8][exitCode:i32be][finalOffset:u64be]
const WS_FRAME_INIT = 1
const WS_FRAME_DATA = 2
const WS_FRAME_MODE_CHANGED = 3
const WS_FRAME_RESIZED = 4
const WS_FRAME_SESSION_ENDED = 5
const WS_FRAME_ERROR = 6
const WS_FRAME_PONG = 7
const WS_FRAME_CONTROL = 8
const WS_FLAG_APP_CURSOR_KEYS = 1 << 0
const WS_FLAG_BRACKETED_PASTE_MODE = 1 << 1
export const WS_INIT_HEADER_LEN = 28
const WS_DATA_HEADER_LEN = 9
const WS_ENDED_LEN = 14

const textDecoder = new TextDecoder()

export type ControlRole = 'controller' | 'observer'

export type ServerFrame =
  | {
      type: 'init'
      appCursorKeys: boolean
      bracketedPasteMode: boolean
      endOffset: number
      incarnation: number
      running: boolean
      attachmentId: number
      role: ControlRole
      data: Uint8Array
    }
  | { type: 'data'; offset: number; data: Uint8Array }
  | { type: 'modeChanged'; appCursorKeys: boolean; bracketedPasteMode: boolean }
  | { type: 'resized'; rows: number; cols: number }
  | { type: 'sessionEnded'; exitCode: number | null; finalOffset: number }
  | { type: 'error'; message: string }
  | { type: 'control'; role: ControlRole }
  | { type: 'pong' }

/** Decode one server frame; `null` for empty, truncated, or unknown frames. */
export function parseServerFrame(bytes: Uint8Array): ServerFrame | null {
  if (bytes.length === 0) return null
  const tag = bytes[0]
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)

  switch (tag) {
    case WS_FRAME_INIT: {
      if (bytes.length < WS_INIT_HEADER_LEN) return null
      const flags = bytes[1]
      return {
        type: 'init',
        appCursorKeys: (flags & WS_FLAG_APP_CURSOR_KEYS) !== 0,
        bracketedPasteMode: (flags & WS_FLAG_BRACKETED_PASTE_MODE) !== 0,
        endOffset: Number(view.getBigUint64(2, false)),
        incarnation: Number(view.getBigUint64(10, false)),
        running: bytes[18] === 1,
        attachmentId: Number(view.getBigUint64(19, false)),
        role: bytes[27] === 1 ? 'controller' : 'observer',
        data: bytes.subarray(WS_INIT_HEADER_LEN),
      }
    }
    case WS_FRAME_DATA: {
      if (bytes.length < WS_DATA_HEADER_LEN) return null
      return {
        type: 'data',
        offset: Number(view.getBigUint64(1, false)),
        data: bytes.subarray(WS_DATA_HEADER_LEN),
      }
    }
    case WS_FRAME_MODE_CHANGED: {
      const flags = bytes[1] ?? 0
      return {
        type: 'modeChanged',
        appCursorKeys: (flags & WS_FLAG_APP_CURSOR_KEYS) !== 0,
        bracketedPasteMode: (flags & WS_FLAG_BRACKETED_PASTE_MODE) !== 0,
      }
    }
    case WS_FRAME_RESIZED: {
      if (bytes.length < 5) return null
      return { type: 'resized', rows: view.getUint16(1, false), cols: view.getUint16(3, false) }
    }
    case WS_FRAME_SESSION_ENDED: {
      const hasExitCode = bytes[1] === 1
      return {
        type: 'sessionEnded',
        exitCode: hasExitCode && bytes.length >= 6 ? view.getInt32(2, false) : null,
        finalOffset: bytes.length >= WS_ENDED_LEN ? Number(view.getBigUint64(6, false)) : 0,
      }
    }
    case WS_FRAME_ERROR:
      return { type: 'error', message: textDecoder.decode(bytes.subarray(1)) }
    case WS_FRAME_CONTROL:
      return { type: 'control', role: bytes[1] === 1 ? 'controller' : 'observer' }
    case WS_FRAME_PONG:
      return { type: 'pong' }
    default:
      return null
  }
}
