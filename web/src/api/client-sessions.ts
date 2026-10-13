/**
 * Sessions API: CRUD, notifications, logs and file upload.
 * Request/token plumbing lives in ./client-http.ts.
 */

import type {
  CreateSessionSpec,
  ListPage,
  LogsResponse,
  SessionSortField,
  SessionStatus,
  SessionSummary,
  SortOrder,
  UpdateSessionMetadataSpec,
} from './types'
import { BASE, getToken, rejectUnauthorized, req } from './client-http'

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

export function fetchSession(
  id: string,
  node?: string,
  signal?: AbortSignal
): Promise<SessionSummary> {
  const q = node ? `?node=${encodeURIComponent(node)}` : ''
  return req<SessionSummary>(`${BASE}/sessions/${id}${q}`, { signal })
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

/** Removes a stopped session, or force-removes a running session when requested. */
export function removeSession(
  id: string,
  node?: string,
  force = false
): Promise<{ removed: boolean }> {
  const params = new URLSearchParams()
  if (force) params.set('force', 'true')
  if (node) params.set('node', node)
  const query = params.size > 0 ? `?${params}` : ''
  return req(`${BASE}/sessions/${encodeURIComponent(id)}${query}`, { method: 'DELETE' })
}

/** Force-removes the session and its files, like `oly rm <id> -f`. */
export function forceRemoveSession(id: string, node?: string): Promise<{ removed: boolean }> {
  return removeSession(id, node, true)
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
    rejectUnauthorized()
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
