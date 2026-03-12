import type {
  SessionSummary,
  SessionStatus,
  ListPage,
  CreateSessionSpec,
  LogsResponse,
  SessionEvent,
  WsServerMessage,
  WsClientMessage,
  PushSubscriptionInput,
  AuthStatus,
  LoginResponse,
  NodeSummary,
  SessionSortField,
  SortOrder,
} from './types.ts'
import { AuthRequiredError, TooManyAttemptsError } from './types.ts'

const BASE = '/api'

// ---------------------------------------------------------------------------
// Token storage (sessionStorage — cleared when the tab closes)
// ---------------------------------------------------------------------------

const TOKEN_KEY = 'oly_auth_token'

export function getToken(): string | null {
  return sessionStorage.getItem(TOKEN_KEY)
}

export function setToken(token: string): void {
  sessionStorage.setItem(TOKEN_KEY, token)
}

export function clearToken(): void {
  sessionStorage.removeItem(TOKEN_KEY)
}

// ---------------------------------------------------------------------------
// REST helpers
// ---------------------------------------------------------------------------

async function req<T>(url: string, init?: RequestInit): Promise<T> {
  const token = getToken()
  const headers = new Headers(init?.headers)
  headers.set('Content-Type', 'application/json')
  if (token) {
    headers.set('Authorization', `Bearer ${token}`)
  }
  const res = await fetch(url, {
    headers,
    ...init,
  })
  if (res.status === 401) {
    clearToken()
    window.dispatchEvent(new CustomEvent('oly:auth-required'))
    throw new AuthRequiredError()
  }
  if (!res.ok) {
    const body = await res.json().catch(() => ({}))
    throw new Error(body?.error ?? `HTTP ${res.status}`)
  }
  return res.json() as Promise<T>
}

// ---------------------------------------------------------------------------
// Auth API
// ---------------------------------------------------------------------------

export function getAuthStatus(): Promise<AuthStatus> {
  return req<AuthStatus>(`${BASE}/auth/status`)
}

export async function login(password: string): Promise<LoginResponse> {
  const res = await fetch(`${BASE}/auth/login`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    credentials: 'omit',
    body: JSON.stringify({ password }),
  })
  if (res.status === 429) {
    const body = await res.json().catch(() => ({}))
    const secs = body?.retry_after_seconds ?? 900
    throw new TooManyAttemptsError(secs)
  }
  if (!res.ok) {
    const body = await res.json().catch(() => ({}))
    // Re-attach attempts_remaining if present for consumers
    const err = new Error(body?.error ?? `HTTP ${res.status}`) as Error & {
      attemptsRemaining?: number
    }
    err.attemptsRemaining = body?.attempts_remaining
    throw err
  }
  return res.json() as Promise<LoginResponse>
}

export async function logout(): Promise<void> {
  const token = getToken()
  if (!token) return
  await fetch(`${BASE}/auth/logout`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
    credentials: 'omit',
  }).catch(() => {
    /* best effort */
  })
  clearToken()
}

// ---------------------------------------------------------------------------
// Sessions API
// ---------------------------------------------------------------------------

export interface ListParams {
  search?: string
  status?: SessionStatus
  limit?: number
  offset?: number
  sort?: SessionSortField
  order?: SortOrder
  /** If set, list sessions on this connected secondary node. */
  node?: string
}

export function fetchSessions(params: ListParams = {}): Promise<ListPage<SessionSummary>> {
  const q = new URLSearchParams()
  if (params.search) q.set('search', params.search)
  if (params.status) q.set('status', params.status)
  if (params.limit != null) q.set('limit', String(params.limit))
  if (params.offset != null) q.set('offset', String(params.offset))
  if (params.sort) q.set('sort', params.sort)
  if (params.order) q.set('order', params.order)
  if (params.node) q.set('node', params.node)
  const qs = q.toString()
  return req<ListPage<SessionSummary>>(`${BASE}/sessions${qs ? `?${qs}` : ''}`)
}

export function fetchSession(id: string, node?: string): Promise<SessionSummary> {
  const q = node ? `?node=${encodeURIComponent(node)}` : ''
  return req<SessionSummary>(`${BASE}/sessions/${id}${q}`)
}

export function startSession(spec: CreateSessionSpec): Promise<{ session_id: string }> {
  return req(`${BASE}/sessions`, { method: 'POST', body: JSON.stringify(spec) })
}

export function stopSession(
  id: string,
  grace_seconds?: number,
  node?: string
): Promise<{ stopped: boolean }> {
  const q = node ? `?node=${encodeURIComponent(node)}` : ''
  return req(`${BASE}/sessions/${id}/stop${q}`, {
    method: 'POST',
    body: JSON.stringify(grace_seconds !== undefined ? { grace_seconds } : {}),
  })
}

export function killSession(id: string, node?: string): Promise<{ killed: boolean }> {
  const q = node ? `?node=${encodeURIComponent(node)}` : ''
  return req(`${BASE}/sessions/${id}/kill${q}`, { method: 'POST', body: '{}' })
}

export function sendInput(id: string, data: string, node?: string): Promise<{ ok: boolean }> {
  const q = node ? `?node=${encodeURIComponent(node)}` : ''
  return req(`${BASE}/sessions/${id}/input${q}`, {
    method: 'POST',
    body: JSON.stringify({ data }),
  })
}

export function fetchLogs(
  id: string,
  opts: { offset?: number; limit?: number } = {},
  node?: string
): Promise<LogsResponse> {
  const q = new URLSearchParams()
  if (opts.offset !== undefined) q.set('offset', String(opts.offset))
  if (opts.limit !== undefined) q.set('limit', String(opts.limit))
  if (node) q.set('node', node)
  const qs = q.toString()
  return req<LogsResponse>(`${BASE}/sessions/${id}/logs${qs ? `?${qs}` : ''}`)
}

// ---------------------------------------------------------------------------
// Nodes API
// ---------------------------------------------------------------------------

export function fetchNodes(): Promise<NodeSummary[]> {
  return req<NodeSummary[]>(`${BASE}/nodes`)
}

export function fetchPushPublicKey(): Promise<{ public_key: string | null }> {
  return req<{ public_key: string | null }>(`${BASE}/push/public-key`)
}

export function upsertPushSubscription(
  subscription: PushSubscriptionInput
): Promise<{ ok: boolean }> {
  return req<{ ok: boolean }>(`${BASE}/push/subscriptions`, {
    method: 'POST',
    body: JSON.stringify(subscription),
  })
}

export function deletePushSubscription(
  endpoint: string
): Promise<{ ok: boolean; deleted: boolean }> {
  return req<{ ok: boolean; deleted: boolean }>(`${BASE}/push/subscriptions`, {
    method: 'DELETE',
    body: JSON.stringify({ endpoint }),
  })
}

// ---------------------------------------------------------------------------
// SSE subscription
// ---------------------------------------------------------------------------

type EventCallback = (ev: SessionEvent) => void
type SseConnectionState = 'live' | 'reconnecting' | 'offline'

export function subscribeEvents(
  cb: EventCallback,
  onStateChange?: (state: SseConnectionState) => void
): () => void {
  let es: EventSource | null = null
  let retryDelay = 1000
  let stopped = false
  let retryTimer: ReturnType<typeof setTimeout> | null = null

  const setState = (state: SseConnectionState) => {
    onStateChange?.(state)
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

    setState('reconnecting')
    es?.close()
    es = null
    const tok = getToken()
    const evUrl = tok
      ? `${BASE}/sessions/events?token=${encodeURIComponent(tok)}`
      : `${BASE}/sessions/events`
    es = new EventSource(evUrl)

    es.addEventListener('snapshot', (e: MessageEvent) => {
      try {
        cb({ event: 'snapshot', data: JSON.parse(e.data) })
      } catch {
        /* ignore */
      }
    })
    es.addEventListener('session_created', (e: MessageEvent) => {
      try {
        cb({ event: 'session_created', data: JSON.parse(e.data) })
      } catch {
        /* ignore */
      }
    })
    es.addEventListener('session_updated', (e: MessageEvent) => {
      try {
        cb({ event: 'session_updated', data: JSON.parse(e.data) })
      } catch {
        /* ignore */
      }
    })
    es.addEventListener('session_deleted', (e: MessageEvent) => {
      try {
        cb({ event: 'session_deleted', data: JSON.parse(e.data) })
      } catch {
        /* ignore */
      }
    })
    es.addEventListener('session_notification', (e: MessageEvent) => {
      try {
        cb({ event: 'session_notification', data: JSON.parse(e.data) })
      } catch {
        /* ignore */
      }
    })

    es.onerror = () => {
      es?.close()
      es = null
      if (!stopped) {
        setState(typeof navigator !== 'undefined' && !navigator.onLine ? 'offline' : 'reconnecting')
        scheduleReconnect()
        retryDelay = Math.min(retryDelay * 2, 30_000)
      }
    }

    es.onopen = () => {
      setState('live')
      retryDelay = 1000
    }
  }

  const handleOnline = () => {
    if (stopped) return
    setState('reconnecting')
    es?.close()
    connect()
  }

  const handleOffline = () => {
    if (stopped) return
    setState('offline')
    es?.close()
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

// ---------------------------------------------------------------------------
// WebSocket PTY attach
// ---------------------------------------------------------------------------

/** Decode a base64 string into a Uint8Array of raw bytes. */
function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64)
  const bytes = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i)
  return bytes
}

export interface AttachOptions {
  /** Called with decoded raw PTY bytes from the initial ring-buffer replay. */
  onInit: (data: Uint8Array, appCursorKeys: boolean, bracketedPasteMode: boolean) => void
  /** Called with decoded raw PTY bytes for each incremental output chunk. */
  onData: (data: Uint8Array) => void
  /** Called when terminal modes change (DECCKM, bracketed paste). */
  onModeChanged: (appCursorKeys: boolean, bracketedPasteMode: boolean) => void
  /** Called when the session ends. */
  onSessionEnded: (exitCode: number | null) => void
  onError: (message: string) => void
  onOpen: () => void
  onClose: (code: number, reason: string) => void
}

export class AttachSocket {
  private ws: WebSocket
  private closed = false

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

    this.ws.onopen = () => opts.onOpen()
    this.ws.onclose = (e) => {
      this.closed = true
      opts.onClose(e.code, e.reason)
    }

    this.ws.onmessage = (e) => {
      try {
        const msg = JSON.parse(e.data) as WsServerMessage
        switch (msg.type) {
          case 'init':
            opts.onInit(base64ToBytes(msg.data), msg.appCursorKeys, msg.bracketedPasteMode)
            break
          case 'data':
            opts.onData(base64ToBytes(msg.data))
            break
          case 'mode_changed':
            opts.onModeChanged(msg.appCursorKeys, msg.bracketedPasteMode)
            break
          case 'session_ended':
            opts.onSessionEnded(msg.exit_code)
            break
          case 'error':
            opts.onError(msg.message)
            break
          case 'pong':
            break
        }
      } catch {
        /* ignore malformed frames */
      }
    }
  }

  private send(msg: WsClientMessage) {
    if (!this.closed && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(msg))
    }
  }

  sendInput(data: string) {
    this.send({ type: 'input', data })
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
