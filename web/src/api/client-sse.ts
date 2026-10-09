/**
 * The shared SSE session event stream, with reconnect and offline handling.
 */

import type { SessionEvent } from './types'
import { BASE, getToken } from './client-http'

type EventCallback = (ev: SessionEvent) => void
/**
 * `connecting` — first attempt only, nothing has gone wrong yet.
 * `reconnecting` — the stream opened before and dropped; retries are backing off.
 * `offline` — the browser reports no network connection.
 */
export type SseConnectionState = 'connecting' | 'live' | 'reconnecting' | 'offline'

export function subscribeEvents(
  cb: EventCallback,
  onStateChange?: (state: SseConnectionState) => void
): () => void {
  let es: EventSource | null = null
  let retryDelay = 1000
  let stopped = false
  let retryTimer: ReturnType<typeof setTimeout> | null = null
  let everOpened = false
  let attempts = 0

  const setState = (state: SseConnectionState) => {
    onStateChange?.(state)
  }

  /**
   * Network is up: a single, first handshake is benign (`connecting`); once the
   * stream has been open before, or the first handshake needed a retry, the
   * pill may honestly report a degraded state (`reconnecting`).
   */
  const setStateForAttempt = () => {
    attempts += 1
    setState(everOpened || attempts > 1 ? 'reconnecting' : 'connecting')
  }

  const scheduleReconnect = () => {
    if (stopped || retryTimer) return
    retryTimer = setTimeout(() => {
      retryTimer = null
      connect()
    }, retryDelay)
  }

  function connect() {
    if (stopped) return
    if (typeof navigator !== 'undefined' && !navigator.onLine) {
      setState('offline')
      scheduleReconnect()
      return
    }

    setStateForAttempt()
    es?.close()
    es = null
    const tok = getToken()
    const evUrl = tok
      ? `${BASE}/sessions/events?token=${encodeURIComponent(tok)}`
      : `${BASE}/sessions/events`
    const source = new EventSource(evUrl)
    es = source

    source.addEventListener('snapshot', (e: MessageEvent) => {
      if (stopped || es !== source) return
      try {
        cb({ event: 'snapshot', data: JSON.parse(e.data) })
      } catch {
        /* ignore */
      }
    })
    source.addEventListener('stream_ready', (e: MessageEvent) => {
      if (stopped || es !== source) return
      try {
        cb({ event: 'stream_ready', data: JSON.parse(e.data) })
      } catch {
        /* ignore */
      }
    })
    source.addEventListener('resync_required', (e: MessageEvent) => {
      if (stopped || es !== source) return
      try {
        cb({ event: 'resync_required', data: JSON.parse(e.data) })
      } catch {
        /* ignore */
      }
    })
    source.addEventListener('node_state', (e: MessageEvent) => {
      if (stopped || es !== source) return
      try {
        cb({ event: 'node_state', data: JSON.parse(e.data) })
      } catch {
        /* ignore */
      }
    })
    source.addEventListener('session_activity', (e: MessageEvent) => {
      if (stopped || es !== source) return
      try {
        cb({ event: 'session_activity', data: JSON.parse(e.data) })
      } catch {
        /* ignore */
      }
    })

    source.addEventListener('session_created', (e: MessageEvent) => {
      if (stopped || es !== source) return
      try {
        cb({ event: 'session_created', data: JSON.parse(e.data) })
      } catch {
        /* ignore */
      }
    })
    source.addEventListener('session_updated', (e: MessageEvent) => {
      if (stopped || es !== source) return
      try {
        cb({ event: 'session_updated', data: JSON.parse(e.data) })
      } catch {
        /* ignore */
      }
    })
    source.addEventListener('session_deleted', (e: MessageEvent) => {
      if (stopped || es !== source) return
      try {
        cb({ event: 'session_deleted', data: JSON.parse(e.data) })
      } catch {
        /* ignore */
      }
    })
    source.addEventListener('session_notification', (e: MessageEvent) => {
      if (stopped || es !== source) return
      try {
        cb({ event: 'session_notification', data: JSON.parse(e.data) })
      } catch {
        /* ignore */
      }
    })

    source.onerror = () => {
      if (stopped || es !== source) return
      source.close()
      es = null
      setState(
        typeof navigator !== 'undefined' && !navigator.onLine
          ? 'offline'
          : everOpened || attempts > 1
            ? 'reconnecting'
            : 'connecting'
      )
      scheduleReconnect()
      retryDelay = Math.min(retryDelay * 2, 30_000)
    }

    source.onopen = () => {
      if (stopped || es !== source) return
      everOpened = true
      setState('live')
      retryDelay = 1000
    }
  }

  const handleOnline = () => {
    if (stopped) return
    // An online notification supersedes a scheduled retry, not an active stream.
    if (es) return
    if (retryTimer) clearTimeout(retryTimer)
    retryTimer = null
    connect()
  }

  const handleOffline = () => {
    if (stopped) return
    setState('offline')
    es?.close()
    es = null
  }

  window.addEventListener('online', handleOnline)
  window.addEventListener('offline', handleOffline)
  connect()
  return () => {
    stopped = true
    if (retryTimer) clearTimeout(retryTimer)
    es?.close()
    window.removeEventListener('online', handleOnline)
    window.removeEventListener('offline', handleOffline)
  }
}
