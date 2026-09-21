import type {
  SessionSummary,
  SessionStatus,
  ListPage,
  CreateSessionSpec,
  LogsResponse,
  SessionEvent,
  WsClientMessage,
  PushSubscriptionInput,
  AuthStatus,
  LoginResponse,
  NodeSummary,
  SessionSortField,
  SortOrder,
  UpdateSessionMetadataSpec,
} from './types.ts'
import { AuthRequiredError, TooManyAttemptsError } from './types.ts'
import { parseServerFrame, type WsModes } from './ws-frames.ts'

const BASE = '/api'

// ---------------------------------------------------------------------------
// Token storage (localStorage — persists across tabs and browser restarts).
// The token itself never expires server-side; it stays valid until the
// password changes or auth is disabled, so we keep it around. The server also
// sets a long-lived HttpOnly cookie on login: it authenticates plain document
// navigations (e.g. /apps/<slug>/ proxy pages) that cannot send headers.
// ---------------------------------------------------------------------------

const TOKEN_KEY = 'oly_auth_token'

export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY)
}

export function setToken(token: string): void {
  localStorage.setItem(TOKEN_KEY, token)
}

export function clearToken(): void {
  localStorage.removeItem(TOKEN_KEY)
}

// ---------------------------------------------------------------------------
// REST helpers
// ---------------------------------------------------------------------------

async function req<T>(url: string, init?: RequestInit): Promise<T> {
  const token = getToken()
  const headers = new Headers(init?.headers)
  const isFormData = typeof FormData !== 'undefined' && init?.body instanceof FormData
  if (!isFormData && !headers.has('Content-Type')) {
    headers.set('Content-Type', 'application/json')
  }
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
    // Keep the default same-origin credentials mode so the browser stores the
    // HttpOnly auth cookie from the Set-Cookie response header. Without it the
    // cookie is dropped and document-level navigations (proxy apps) 401.
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

export function updateSessionMetadata(
  id: string,
  spec: UpdateSessionMetadataSpec,
  node?: string
): Promise<SessionSummary> {
  const q = node ? `?node=${encodeURIComponent(node)}` : ''
  return req<SessionSummary>(`${BASE}/sessions/${id}/metadata${q}`, {
    method: 'POST',
    body: JSON.stringify(spec),
  })
}

export function setSessionNotifications(
  id: string,
  enabled: boolean,
  node?: string
): Promise<{ ok: boolean; notifications_enabled: boolean }> {
  const q = node ? `?node=${encodeURIComponent(node)}` : ''
  return req<{ ok: boolean; notifications_enabled: boolean }>(
    `${BASE}/sessions/${id}/notifications${q}`,
    {
      method: 'POST',
      body: JSON.stringify({ enabled }),
    }
  )
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

export interface UploadSessionFileResponse {
  ok: boolean
  path: string
  bytes: number
}

export function uploadSessionFile(
  id: string,
  file: File,
  node?: string
): Promise<UploadSessionFileResponse> {
  const q = node ? `?node=${encodeURIComponent(node)}` : ''
  const body = new FormData()
  body.set('file', file, file.name)
  return req<UploadSessionFileResponse>(`${BASE}/sessions/${id}/upload${q}`, {
    method: 'POST',
    body,
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

export interface LogsTailResponse {
  output: Uint8Array
  resizes: { offset: number; rows: number; cols: number }[]
}

export async function fetchLogsTail(
  id: string,
  tail: number,
  cols: number,
  node?: string
): Promise<LogsTailResponse> {
  const q = new URLSearchParams()
  q.set('tail', String(tail))
  q.set('cols', String(cols))
  if (node) q.set('node', node)
  const qs = q.toString()

  const token = getToken()
  const headers = new Headers()
  if (token) headers.set('Authorization', `Bearer ${token}`)

  const res = await fetch(`${BASE}/sessions/${id}/logs/tail?${qs}`, { headers })
  if (res.status === 401) {
    clearToken()
    window.dispatchEvent(new CustomEvent('oly:auth-required'))
    throw new AuthRequiredError()
  }
  if (!res.ok) {
    const body = await res.json().catch(() => ({}))
    throw new Error(body?.error ?? `HTTP ${res.status}`)
  }

  const resizesHeader = res.headers.get('x-log-resizes')
  const resizes = resizesHeader ? JSON.parse(resizesHeader) : []
  const buf = await res.arrayBuffer()
  return { output: new Uint8Array(buf), resizes }
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
export type SseConnectionState = 'live' | 'reconnecting' | 'offline'

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

// Binary attach frames (M3-3): every data-carrying frame names its stream
// cursor so the client can verify the C/C+1 boundary — contiguous,
// gap-free application of the canonical stream (PLAN §7.2, invariant I2).
// Decoding lives in ./ws-frames.ts so the wire format is pinned against the
// shared fixture tests/fixtures/ws_frames.json (encoder and decoder can
// never drift apart without a test failing).

/** Applied-cursor credit cadence (M3-5): at most one ack per MiB. */
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
  /** Called on control handoffs with this attachment's new role. */
  onControl?: (role: 'controller' | 'observer') => void
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
  /** Granted control role; observers never send input or resize (I6). */
  role: 'controller' | 'observer' = 'controller'

  constructor(
    sessionId: string,
    opts: AttachOptions,
    node?: string,
    initialSize?: { rows: number; cols: number },
    controlRole?: 'observer' | 'controller' | 'takeover'
  ) {
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:'
    const host = location.host
    const params = new URLSearchParams()
    if (node) params.set('node', node)
    if (initialSize && initialSize.rows > 0 && initialSize.cols > 0) {
      params.set('rows', String(initialSize.rows))
      params.set('cols', String(initialSize.cols))
    }
    if (controlRole) params.set('role', controlRole)
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
            this.role = frame.role
            opts.onControl?.(this.role)
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
              // A gap or overlap means the rendered screen would be corrupt;
              // abort loudly instead of applying out-of-order bytes (I2).
              opts.onError(
                `attach stream cursor mismatch: expected offset ${this.expectedOffset}, chunk starts at ${frame.offset}`
              )
              this.ws.close()
              return
            }
            this.expectedOffset = frame.offset + frame.data.length
            opts.onData(frame.data)
            // Applied-cursor credit (M3-5): bytes handed to the renderer
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
          case 'control':
            // Control handoff notice for this attachment.
            this.role = frame.role
            opts.onControl?.(this.role)
            return
          case 'pong':
            return
        }
      } catch (err) {
        // Truncated/unknown frames are corruption or a version mismatch:
        // surface them instead of silently dropping stream bytes (I2).
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
    if (this.role !== 'controller') return
    this.send({ type: 'input', data, waitForChange })
  }
  /** Request the control lease (observer → controller takeover). */
  sendAcquireControl() {
    this.send({ type: 'acquire_control' })
  }

  /** Send an applied-cursor credit (M3-5, I7); throttled by the caller. */
  sendAck(offset: number) {
    this.send({ type: 'ack', offset })
  }
  sendBusy() {
    this.send({ type: 'busy' })
  }
  sendResize(rows: number, cols: number) {
    if (this.role !== 'controller') return
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
