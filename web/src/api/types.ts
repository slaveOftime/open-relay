// ---------------------------------------------------------------------------
// Session types (mirrors Rust protocol.rs)
// ---------------------------------------------------------------------------

export const SESSION_STATUSES = [
  'created',
  'running',
  'stopping',
  'stopped',
  'killed',
  'failed',
] as const

export type SessionStatus = (typeof SESSION_STATUSES)[number]

export const SESSION_STATUS_FILTERS = ['all', ...SESSION_STATUSES] as const

export type SessionStatusFilter = (typeof SESSION_STATUS_FILTERS)[number]

export const SessionSortField = {
  Id: 'id',
  Title: 'title',
  Command: 'command',
  Cwd: 'cwd',
  Status: 'status',
  Pid: 'pid',
  CreatedAt: 'created_at',
} as const

export type SessionSortField = (typeof SessionSortField)[keyof typeof SessionSortField]

export const SortOrder = {
  Asc: 'asc',
  Desc: 'desc',
} as const

export type SortOrder = (typeof SortOrder)[keyof typeof SortOrder]

const sessionSortFields = new Set<string>(Object.values(SessionSortField))
const sortOrders = new Set<string>(Object.values(SortOrder))
const sessionStatuses = new Set<string>(SESSION_STATUSES)
const sessionStatusFilters = new Set<string>(SESSION_STATUS_FILTERS)

export function isSessionSortField(value: unknown): value is SessionSortField {
  return typeof value === 'string' && sessionSortFields.has(value)
}

export function isSortOrder(value: unknown): value is SortOrder {
  return typeof value === 'string' && sortOrders.has(value)
}

export function isSessionStatus(value: unknown): value is SessionStatus {
  return typeof value === 'string' && sessionStatuses.has(value)
}

export function isSessionStatusFilter(value: unknown): value is SessionStatusFilter {
  return typeof value === 'string' && sessionStatusFilters.has(value)
}

export interface SessionSummary {
  id: string
  title: string | null
  tags: string[]
  command: string
  args: string[]
  pid: number | null
  status: SessionStatus
  created_at: string // ISO 8601
  started_at: string | null // ISO 8601
  ended_at: string | null // ISO 8601
  cwd: string | null
  input_needed: boolean
  notifications_enabled: boolean
  node?: string | null
  last_total_bytes: number
}

export interface CreateSessionSpec {
  cmd: string
  args?: string[]
  title?: string
  tags?: string[]
  cwd?: string
  rows?: number
  cols?: number
  /** If set, create the session on this connected secondary node. */
  node?: string
}

// ---------------------------------------------------------------------------
// Node federation types
// ---------------------------------------------------------------------------

export interface NodeSummary {
  name: string
  connected: boolean
}

export interface LogsResponse {
  chunks: string[]
  total: number
  resizes: LogResizeEvent[]
}

export interface LogResizeEvent {
  offset: number
  rows: number
  cols: number
}

export interface ListPage<T> {
  items: T[]
  total: number
  offset: number
  limit: number
}

export interface PushSubscriptionKeys {
  auth: string
  p256dh: string
}

export interface PushSubscriptionInput {
  endpoint: string
  keys: PushSubscriptionKeys
}

// ---------------------------------------------------------------------------
// SSE event types
// ---------------------------------------------------------------------------

export type SessionEvent =
  | { event: 'snapshot'; data: SessionSummary[] }
  | { event: 'session_created'; data: SessionSummary }
  | { event: 'session_updated'; data: SessionSummary }
  | { event: 'session_deleted'; data: { id: string; node?: string | null } }
  | {
      event: 'session_notification'
      data: SessionNotificationData
    }

export type SessionNotificationData = {
  kind: string
  title: string
  description: string
  body: string
  navigation_url?: string
  session_ids: string[]
  trigger_rule?: string
  trigger_detail?: string
  node?: string | null
  last_total_bytes: number
}

// ---------------------------------------------------------------------------
// WebSocket protocol
// ---------------------------------------------------------------------------

export type WsServerMessage =
  | { type: 'init'; data: string; appCursorKeys: boolean; bracketedPasteMode: boolean }
  | { type: 'data'; data: string }
  | { type: 'mode_changed'; appCursorKeys: boolean; bracketedPasteMode: boolean }
  | { type: 'resized'; rows: number; cols: number }
  | { type: 'session_ended'; exit_code: number | null }
  | { type: 'error'; message: string }
  | { type: 'pong' }

export type WsClientMessage =
  | { type: 'input'; data: string }
  | { type: 'resize'; rows: number; cols: number }
  | { type: 'detach' }

// ---------------------------------------------------------------------------
// Auth types
// ---------------------------------------------------------------------------

export interface AuthStatus {
  auth_required: boolean
}

export interface LoginResponse {
  token: string
}

/** Thrown by req<T>() when the server returns 401 Unauthorized. */
export class AuthRequiredError extends Error {
  constructor() {
    super('authentication required')
    this.name = 'AuthRequiredError'
  }
}

/** Thrown by login() when the server returns 429 Too Many Requests. */
export class TooManyAttemptsError extends Error {
  retryAfterSeconds: number
  constructor(retryAfterSeconds: number) {
    super('too many failed login attempts')
    this.name = 'TooManyAttemptsError'
    this.retryAfterSeconds = retryAfterSeconds
  }
}
