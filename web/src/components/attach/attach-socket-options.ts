/**
 * The `AttachSocket` options object for the attach terminal.
 *
 * This is the wiring between the daemon's frames and the terminal: init writes
 * the snapshot, data appends to it, mode changes re-mirror the child's request,
 * a resize is pushed back at the PTY, and end/error frames stop the world. Split
 * out of `SessionDetailPage` because it is a long but shallow translation table,
 * and because the two flags it needs (`ended`, `gotSnapshot`) can live inside it
 * rather than in the page's hook list.
 *
 * `discarded` deliberately does *not* live here: the page's cleanup owns it,
 * because it has to know whether *this* run is still current.
 */

import type { RefObject } from 'react'

import type { SessionSummary } from '@/api/types'
import { terminalModeSequences } from '@/api/ws-frames'
import type { AttachOptions, AttachSocket } from '@/api/client'
import { fetchSession } from '@/api/client'
import type { XTermHandle } from '@/components/terminal/XTerm'

const modesEncoder = new TextEncoder()

export interface AttachSocketHandlerDeps {
  sessionId: string
  node?: string
  pushTrace: (step: string) => void
  /** Writes PTY bytes to the terminal. `reset` seeds the scrollback. */
  enqueueOutput: (chunks: Uint8Array[], opts?: { reset?: boolean }) => void
  /** Marks that a frame arrived, for the stall watchdog. */
  noteFrame: () => void
  /** Marks user-visible activity so the idle border restarts. */
  noteActivity: () => void
  termRef: RefObject<XTermHandle | null>
  isMounted: RefObject<boolean>
  setError: (message: string | null) => void
  setExitCode: (code: number | null | undefined) => void
  setConnected: (connected: boolean) => void
  /** Records that the socket has opened at least once (drives the header label). */
  setEverConnected: () => void
  /** Switches the page to the log view once the session is over. */
  openLogsView: () => void
  setSession: (session: SessionSummary) => void
  /** Called after the built-in open bookkeeping, for reconnect state. */
  afterOpen?: () => void
  /** Called on close with whether the session had ended, plus close details. */
  afterClose?: (ended: boolean, code: number, reason: string) => void
}

/** Builds the options and keeps the `ended` flag private to them. */
export function buildAttachSocketOptions(deps: AttachSocketHandlerDeps): AttachOptions {
  const {
    sessionId,
    node,
    pushTrace,
    enqueueOutput,
    noteFrame,
    noteActivity,
    termRef,
    isMounted,
    setError,
    setExitCode,
    setConnected,
    setEverConnected,
    openLogsView,
    setSession,
    afterOpen,
    afterClose,
  } = deps

  // Server sent an 'end' frame: the session finished normally, so a close that
  // follows is not a drop and must not reconnect.
  let ended = false
  let gotSnapshot = false

  return {
    onOpen: () => {
      noteFrame()
      pushTrace('websocket open')
      setError(null)
      setEverConnected()
      if (isMounted.current) setConnected(true)
      noteActivity()
      afterOpen?.()
    },

    onInit: (data, modes) => {
      noteFrame()
      if (!gotSnapshot) {
        pushTrace(`init received (${data.length} bytes)`)
        gotSnapshot = true
      }
      enqueueOutput([data], { reset: true })
      // Mirror the child's input modes right after the snapshot — the web
      // equivalent of the native client's sync_local_terminal_modes(). The
      // snapshot stream never replays DECSET mode sequences (and the scrollback
      // seed can carry stale ones), so without this xterm.js would never capture
      // mouse clicks/wheel for a program that had them enabled before the page
      // loaded.
      enqueueOutput([modesEncoder.encode(terminalModeSequences(modes))])
    },

    onData: (data) => {
      noteFrame()
      enqueueOutput([data])
    },

    onModeChanged: (modes) => {
      noteFrame()
      // Modes are authoritative server-side; re-mirror them so the terminal's
      // capture state always matches the child's request even after a reconnect
      // or stale replay bytes.
      enqueueOutput([modesEncoder.encode(terminalModeSequences(modes))])
    },

    onResized: (rows, cols) => {
      noteFrame()
      // If the PTY was resized to dimensions that don't match our viewport (e.g.
      // a CLI client resized), push our actual size back so the PTY adapts to
      // the web client.
      termRef.current?.resize(cols, rows)
    },

    onSessionEnded: (code) => {
      noteFrame()
      ended = true
      noteActivity()
      pushTrace(`server end frame received (exit=${code ?? 'null'})`)
      if (!isMounted.current) return
      const exitMsg = code != null ? ` (exit code: ${code})` : ''
      termRef.current?.writeln(`\r\n\x1b[2m[Session ended${exitMsg}]\x1b[0m`)
      setExitCode(code)
      setConnected(false)
      openLogsView()
      fetchSession(sessionId, node ?? undefined)
        .then((session) => {
          if (isMounted.current) setSession(session)
        })
        .catch(() => {})
    },

    onError: (message) => {
      noteFrame()
      pushTrace(`server error frame: ${message}`)
      if (!isMounted.current) return
      termRef.current?.writeln(`\r\n\x1b[31mError: ${message}\x1b[0m`)
      setError(`Server error: ${message}`)
    },

    onClose: (code, reason) => {
      noteFrame()
      noteActivity()
      pushTrace(`websocket close (code=${code}${reason ? ` reason=${reason}` : ''})`)
      if (isMounted.current) setConnected(false)
      afterClose?.(ended, code, reason)
    },
  }
}

/** A socket that has already been created, for callers that only need its type. */
export type AttachSocketInstance = AttachSocket
