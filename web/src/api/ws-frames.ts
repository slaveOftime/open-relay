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
// MODECHG:  [tag=3][flags:u8]
// flags: bit0 appCursorKeys, bit1 bracketedPasteMode, bit2 mouseReport,
//        bit3 sgrMouse, bit4 focusEvents — the child's input modes,
//        mirrored into xterm.js via terminalModeSequences().
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
const WS_FLAG_MOUSE_REPORT = 1 << 2
const WS_FLAG_SGR_MOUSE = 1 << 3
const WS_FLAG_FOCUS_EVENTS = 1 << 4
export const WS_INIT_HEADER_LEN = 28
const WS_DATA_HEADER_LEN = 9
const WS_ENDED_LEN = 14

const textDecoder = new TextDecoder()

export type ControlRole = 'controller' | 'observer'

/**
 * Input-affecting terminal modes carried by init/modeChanged frames.
 * The daemon tracks them in the engine; the browser mirrors them into
 * xterm.js via `terminalModeSequences`, the web equivalent of the
 * native client's `sync_local_terminal_modes` — without it a fresh page
 * load into an already-mouse-enabled program (vim, htop, …) would leave
 * xterm.js capturing nothing, because the snapshot stream deliberately
 * omits mode sequences.
 */
export interface WsModes {
  appCursorKeys: boolean
  bracketedPasteMode: boolean
  /** Child enabled mouse reporting (any of 1000/1002/1003). */
  mouseReport: boolean
  /** Child negotiated SGR (1006) mouse encoding. */
  sgrMouse: boolean
  /** Child enabled focus in/out reporting (1004). */
  focusEvents: boolean
}

/**
 * Authoritative DECSET byte stream that pins a terminal to the given
 * modes. Every affected mode is set or cleared explicitly (never
 * left to history), so stale mode toggles replayed from the scrollback
 * seed are overwritten rather than trusted.
 */
export function terminalModeSequences(modes: WsModes): string {
  let seq = modes.appCursorKeys ? '\x1b[?1h' : '\x1b[?1l'
  seq += modes.bracketedPasteMode ? '\x1b[?2004h' : '\x1b[?2004l'
  // Clear every mouse capture mode before re-asserting the negotiated
  // one; the daemon collapses 1000/1002/1003 into a single flag, so the
  // browser enables plain button tracking (clicks + wheel) and lets the
  // raw byte stream refine it if the child changes its mind live.
  seq += '\x1b[?1000l\x1b[?1002l\x1b[?1003l'
  if (modes.mouseReport) seq += '\x1b[?1000h'
  seq += modes.sgrMouse ? '\x1b[?1006h' : '\x1b[?1006l'
  seq += modes.focusEvents ? '\x1b[?1004h' : '\x1b[?1004l'
  return seq
}

export type ServerFrame =
  | {
      type: 'init'
      appCursorKeys: boolean
      bracketedPasteMode: boolean
      mouseReport: boolean
      sgrMouse: boolean
      focusEvents: boolean
      endOffset: number
      incarnation: number
      running: boolean
      attachmentId: number
      role: ControlRole
      data: Uint8Array
    }
  | { type: 'data'; offset: number; data: Uint8Array }
  | {
      type: 'modeChanged'
      appCursorKeys: boolean
      bracketedPasteMode: boolean
      mouseReport: boolean
      sgrMouse: boolean
      focusEvents: boolean
    }
  | { type: 'resized'; rows: number; cols: number }
  | { type: 'sessionEnded'; exitCode: number | null; finalOffset: number }
  | { type: 'error'; message: string }
  | { type: 'control'; role: ControlRole }
  | { type: 'pong' }

/**
 * Decode one server frame. `null` means "no frame" (empty payload) and is
 * the only ignorable result: truncated headers and unknown tags throw,
 * because silently dropping a frame desynchronizes the attach stream —
 * the client must surface the corruption instead of rendering on
 * (fail-loud, I2).
 */
export function parseServerFrame(bytes: Uint8Array): ServerFrame | null {
  if (bytes.length === 0) return null
  const tag = bytes[0]
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)

  switch (tag) {
    case WS_FRAME_INIT: {
      if (bytes.length < WS_INIT_HEADER_LEN)
        throw new Error(`truncated init frame: ${bytes.length} bytes`)
      const flags = bytes[1]
      return {
        type: 'init',
        appCursorKeys: (flags & WS_FLAG_APP_CURSOR_KEYS) !== 0,
        bracketedPasteMode: (flags & WS_FLAG_BRACKETED_PASTE_MODE) !== 0,
        mouseReport: (flags & WS_FLAG_MOUSE_REPORT) !== 0,
        sgrMouse: (flags & WS_FLAG_SGR_MOUSE) !== 0,
        focusEvents: (flags & WS_FLAG_FOCUS_EVENTS) !== 0,
        endOffset: Number(view.getBigUint64(2, false)),
        incarnation: Number(view.getBigUint64(10, false)),
        running: bytes[18] === 1,
        attachmentId: Number(view.getBigUint64(19, false)),
        role: bytes[27] === 1 ? 'controller' : 'observer',
        data: bytes.subarray(WS_INIT_HEADER_LEN),
      }
    }
    case WS_FRAME_DATA: {
      if (bytes.length < WS_DATA_HEADER_LEN)
        throw new Error(`truncated data frame: ${bytes.length} bytes`)
      return {
        type: 'data',
        offset: Number(view.getBigUint64(1, false)),
        data: bytes.subarray(WS_DATA_HEADER_LEN),
      }
    }
    case WS_FRAME_MODE_CHANGED: {
      if (bytes.length < 2) throw new Error('truncated mode-changed frame')
      const flags = bytes[1]
      return {
        type: 'modeChanged',
        appCursorKeys: (flags & WS_FLAG_APP_CURSOR_KEYS) !== 0,
        bracketedPasteMode: (flags & WS_FLAG_BRACKETED_PASTE_MODE) !== 0,
        mouseReport: (flags & WS_FLAG_MOUSE_REPORT) !== 0,
        sgrMouse: (flags & WS_FLAG_SGR_MOUSE) !== 0,
        focusEvents: (flags & WS_FLAG_FOCUS_EVENTS) !== 0,
      }
    }
    case WS_FRAME_RESIZED: {
      if (bytes.length < 5) throw new Error(`truncated resized frame: ${bytes.length} bytes`)
      return { type: 'resized', rows: view.getUint16(1, false), cols: view.getUint16(3, false) }
    }
    case WS_FRAME_SESSION_ENDED: {
      if (bytes.length < 2) throw new Error('truncated session-ended frame')
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
      if (bytes.length < 2) throw new Error('truncated control frame')
      return { type: 'control', role: bytes[1] === 1 ? 'controller' : 'observer' }
    case WS_FRAME_PONG:
      return { type: 'pong' }
    default:
      throw new Error(`unknown server frame tag ${tag}`)
  }
}
