/**
 * The reconnect scheduler for the attach socket.
 *
 * Owns the attempt counter, the retry timer and the "deferred until the page is
 * usable again" state, so the socket effect can just say "we dropped,
 * reconnect please". The policy itself lives in `lib/attach-reconnect-policy`.
 *
 * The hook owns its refs rather than taking them, so the page still reads
 * `reconnectAttemptRef` / `pendingReconnectRef` where it always did.
 */

import { useCallback, useRef } from 'react'

import {
  deferredReconnectReason,
  isTransientClose,
  reconnectDelayMs,
  shouldDeferReconnect,
} from '@/lib/attach-reconnect-policy'

export interface AttachReconnectCallbacks {
  /** Human-readable trace line for the connect dialog. */
  pushTrace: (step: string) => void
  /** Show or clear the connection error banner. */
  setError: (message: string | null) => void
  /** A reconnect attempt means the connecting flag is true. */
  setConnecting: (connecting: boolean) => void
  /** Ask the caller to rebuild the socket (bumps the reconnect key). */
  triggerReconnect: () => void
  /** True while the owning effect run is still current. */
  isCurrent: () => boolean
}

export interface AttachReconnect {
  /** Attempts made so far; reset on a successful open. */
  attemptRef: React.RefObject<number>
  /** The pending retry timer, if any. */
  timerRef: React.RefObject<number | null>
  /** Set when a reconnect was deferred to the next foreground/online event. */
  pendingRef: React.RefObject<boolean>
  /** A connection opened: forget the backoff and any deferral. */
  resetAttempts: () => void
  /**
   * The socket closed. Schedules a retry, or defers it while the page is hidden
   * or offline. `code`/`reason` are only used for the trace.
   */
  scheduleReconnect: (code: number, reason: string) => void
  /** Drop any pending retry timer. */
  cancelReconnect: () => void
}

export function useAttachReconnect(callbacks: AttachReconnectCallbacks): AttachReconnect {
  const { pushTrace, setError, setConnecting, triggerReconnect, isCurrent } = callbacks

  const attemptRef = useRef(0)
  const timerRef = useRef<number | null>(null)
  const pendingRef = useRef(false)

  const cancelReconnect = useCallback(() => {
    if (timerRef.current !== null) {
      window.clearTimeout(timerRef.current)
      timerRef.current = null
    }
  }, [])

  const resetAttempts = useCallback(() => {
    attemptRef.current = 0
    pendingRef.current = false
  }, [])

  const scheduleReconnect = useCallback(
    (code: number, reason: string) => {
      setConnecting(true)
      const attempt = attemptRef.current + 1
      attemptRef.current = attempt
      const delay = reconnectDelayMs(attempt)
      const hidden = document.visibilityState !== 'visible'
      const offline = typeof navigator !== 'undefined' && !navigator.onLine

      cancelReconnect()

      if (shouldDeferReconnect({ hidden, offline })) {
        pendingRef.current = true
        pushTrace(
          `reconnect deferred (${deferredReconnectReason({ hidden, offline })}) attempt=${attempt}`
        )
        setError(null)
        return
      }

      pendingRef.current = false
      if (!isTransientClose(code)) {
        pushTrace(
          `non-transient close treated as retryable (code=${code}${
            reason ? ` reason=${reason}` : ''
          }) attempt=${attempt}`
        )
      }
      setError(null)

      timerRef.current = window.setTimeout(() => {
        timerRef.current = null
        if (isCurrent()) {
          pushTrace(`retry timer fired (attempt=${attempt}) -> reconnect`)
          triggerReconnect()
        }
      }, delay)
    },
    [cancelReconnect, isCurrent, pushTrace, setConnecting, setError, triggerReconnect]
  )

  return { attemptRef, timerRef, pendingRef, resetAttempts, scheduleReconnect, cancelReconnect }
}
