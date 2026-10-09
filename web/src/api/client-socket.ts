/**
 * WebSocket PTY attach.
 *
 * Binary attach frames: every data-carrying frame names its stream
 * cursor so the client can verify the C/C+1 boundary — contiguous,
 * gap-free application of the canonical stream — every chunk's offset
 * must equal the previous chunk's offset plus its byte length.
 * Decoding lives in ./ws-frames.ts so the wire format is pinned against the
 * shared fixture tests/fixtures/ws_frames.json (encoder and decoder can
 * never drift apart without a test failing).
 */

import type { WsModes } from './ws-frames'
import { parseServerFrame } from './ws-frames'
import type { WsClientMessage } from './types'
import { getToken } from './client-http'

/** Applied-cursor credit cadence: at most one ack per MiB. */
const WS_ACK_STRIDE_BYTES = 1024 * 1024

export interface AttachOptions {
  /** Called with decoded terminal bytes that recreate the current visible session state, plus the authoritative input modes. */
  onInit: (data: Uint8Array, modes: WsModes) => void
  /** Called with decoded raw PTY bytes for each incremental output chunk. */
  onData: (data: Uint8Array) => void
  /** Called when terminal modes change (DECCKM, bracketed paste, mouse, focus). */
  onModeChanged: (modes: WsModes) => void
  /** Called when the PTY was resized by another attached client. */
  onResized?: (rows: number, cols: number) => void
  /** Called when the session ends. */
  onSessionEnded: (exitCode: number | null) => void
  onError: (message: string) => void
  onOpen: () => void
  onClose: (code: number, reason: string) => void
}

export class AttachSocket {
  private ws: WebSocket
  private closed = false
  /** Next expected stream offset (from the init frame's snapshot boundary). */
  private expectedOffset: number | null = null
  private lastAckedOffset = 0

  constructor(
    sessionId: string,
    opts: AttachOptions,
    node?: string,
    initialSize?: { rows: number; cols: number }
  ) {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:'
    const host = location.host
    const params = new URLSearchParams()
    if (node) params.set('node', node)
    if (initialSize && initialSize.rows > 0 && initialSize.cols > 0) {
      params.set('rows', String(initialSize.rows))
      params.set('cols', String(initialSize.cols))
    }
    const tok = getToken()
    if (tok) params.set('token', tok)
    const qs = params.toString()
    const url = `${proto}//${host}/api/sessions/${sessionId}/attach${qs ? `?${qs}` : ''}`
    this.ws = new WebSocket(url)
    this.ws.binaryType = 'arraybuffer'

    this.ws.onopen = () => opts.onOpen()
    this.ws.onclose = (e) => {
      this.closed = true
      opts.onClose(e.code, e.reason)
    }

    this.ws.onmessage = (e) => {
      try {
        if (!(e.data instanceof ArrayBuffer)) return
        const frame = parseServerFrame(new Uint8Array(e.data))
        if (!frame) return

        switch (frame.type) {
          case 'init': {
            this.expectedOffset = frame.endOffset
            opts.onInit(frame.data, {
              appCursorKeys: frame.appCursorKeys,
              bracketedPasteMode: frame.bracketedPasteMode,
              mouseReport: frame.mouseReport,
              sgrMouse: frame.sgrMouse,
              focusEvents: frame.focusEvents,
            })
            return
          }
          case 'data': {
            if (this.expectedOffset !== null && frame.offset !== this.expectedOffset) {
              // A gap or overlap corrupts the rendered screen, so abort
              // loudly rather than apply out-of-order bytes — rejecting
              // is strictly better than papering over.
              opts.onError(
                `attach stream cursor mismatch: expected offset ${this.expectedOffset}, chunk starts at ${frame.offset}`
              )
              this.ws.close()
              return
            }
            this.expectedOffset = frame.offset + frame.data.length
            opts.onData(frame.data)
            // Applied-cursor credit: bytes handed to the renderer
            // count as applied; credit at most once per MiB.
            if (this.expectedOffset >= this.lastAckedOffset + WS_ACK_STRIDE_BYTES) {
              this.lastAckedOffset = this.expectedOffset
              this.sendAck(this.expectedOffset)
            }
            return
          }
          case 'modeChanged':
            opts.onModeChanged({
              appCursorKeys: frame.appCursorKeys,
              bracketedPasteMode: frame.bracketedPasteMode,
              mouseReport: frame.mouseReport,
              sgrMouse: frame.sgrMouse,
              focusEvents: frame.focusEvents,
            })
            return
          case 'resized':
            opts.onResized?.(frame.rows, frame.cols)
            return
          case 'sessionEnded': {
            if (
              frame.finalOffset !== 0 &&
              this.expectedOffset !== null &&
              frame.finalOffset !== this.expectedOffset
            ) {
              opts.onError(
                `attach stream ended at offset ${frame.finalOffset} but the client applied up to ${this.expectedOffset}`
              )
            }
            opts.onSessionEnded(frame.exitCode)
            return
          }
          case 'error':
            opts.onError(frame.message)
            return
          case 'pong':
            return
        }
      } catch (err) {
        // Truncated/unknown frames are corruption or a version mismatch:
        // surface them instead of silently dropping stream bytes.
        opts.onError(`unreadable server frame: ${err instanceof Error ? err.message : String(err)}`)
      }
    }
  }

  private send(msg: WsClientMessage) {
    if (!this.closed && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(msg))
    }
  }

  sendInput(data: string, waitForChange: boolean) {
    this.send({ type: 'input', data, waitForChange })
  }

  /** Send an applied-cursor credit; throttled by the caller. */
  sendAck(offset: number) {
    this.send({ type: 'ack', offset })
  }
  sendBusy() {
    this.send({ type: 'busy' })
  }
  sendResize(rows: number, cols: number) {
    this.send({ type: 'resize', rows, cols })
  }

  /** Detach: session keeps running, WebSocket closes gracefully. */
  detach() {
    this.send({ type: 'detach' })
    this.ws.close(1000, 'detach')
  }

  /** Close the socket without sending detach. Session keeps running. */
  close() {
    if (!this.closed) this.ws.close(1000, 'page-close')
  }
}
