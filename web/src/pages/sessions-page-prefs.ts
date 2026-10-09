/**
 * Sessions-list preferences and the column-settings cache.
 *
 * Both are `localStorage`-backed and both are read once at mount to seed
 * component state, so they are pure functions over a JSON blob rather than
 * React state. Everything degrades to a default: a corrupt entry, a missing
 * `localStorage`, or an impossible value (a page size of 0 or of 10,000)
 * falls back instead of throwing, because losing preferences must not stop
 * the list from rendering.
 */

import {
  isSessionSortField,
  isSessionStatusFilter,
  isSortOrder,
  SessionSortField,
  SortOrder,
  type SessionStatusFilter,
  type SessionSummary,
} from '@/api/types'
import {
  coerceSessionTableColumnSettings,
  SESSION_TABLE_COLUMN_STORAGE_KEY,
  type SessionTableColumnOrder,
  type SessionTableColumnSizes,
} from '@/lib/sessions-table-columns'
import { matchesStatusFilter, normalizeStoredNode } from './sessions-page-events'
import type { GroupBy } from '@/components/sessions/group-by'

export const PREFS_KEY = 'open-relay.webv2.sessions.preferences.v1'
export const LEGACY_PREFS_KEY = 'open-relay.sessions.preferences.v1'
export const DEFAULT_PAGE_SIZE = 15
export const PAGE_SIZE_OPTIONS = [10, 15, 25, 50, 100] as const

export type SessionPrefs = {
  search: string
  statusFilter: SessionStatusFilter
  groupBy: GroupBy
  node: string | null
  sortField: SessionSortField
  sortOrder: SortOrder
  pageSize: number
}

export type LoadErrorState = {
  title: string
  message: string
}

/** `background` pulls swallow errors; `reportError` surfaces them even then. */
export type LoadOptions = { background?: boolean; reportError?: boolean }

export function normalizeStatusFilter(value: unknown): SessionStatusFilter {
  return isSessionStatusFilter(value) ? value : 'all'
}

export function filterSessionsByStatus(
  items: SessionSummary[],
  statusFilter: SessionStatusFilter
): SessionSummary[] {
  if (statusFilter === 'all') return items
  return items.filter((item) => matchesStatusFilter(statusFilter, item.status))
}

export const SORT_OPTIONS: Array<{ label: string; value: SessionSortField }> = [
  { label: 'Created At', value: SessionSortField.CreatedAt },
  { label: 'Status', value: SessionSortField.Status },
  { label: 'Title', value: SessionSortField.Title },
  { label: 'ID', value: SessionSortField.Id },
  { label: 'Command', value: SessionSortField.Command },
  { label: 'CWD', value: SessionSortField.Cwd },
  { label: 'PID', value: SessionSortField.Pid },
]

export function loadSessionPrefs(): SessionPrefs {
  const defaults: SessionPrefs = {
    search: '',
    statusFilter: 'all',
    groupBy: 'none',
    node: null,
    sortField: SessionSortField.CreatedAt,
    sortOrder: SortOrder.Desc,
    pageSize: DEFAULT_PAGE_SIZE,
  }
  if (typeof window === 'undefined') return defaults
  try {
    const raw =
      window.localStorage.getItem(PREFS_KEY) ?? window.localStorage.getItem(LEGACY_PREFS_KEY)
    if (!raw) return defaults
    const parsed = JSON.parse(raw) as Partial<SessionPrefs>
    const groupBy = parsed.groupBy
    const node = parsed.node
    const sortField = parsed.sortField
    const sortOrder = parsed.sortOrder
    return {
      search: typeof parsed.search === 'string' ? parsed.search : defaults.search,
      statusFilter: normalizeStatusFilter(parsed.statusFilter),
      groupBy:
        groupBy === 'none' || groupBy === 'cwd' || groupBy === 'command' || groupBy === 'tag'
          ? groupBy
          : defaults.groupBy,
      node: normalizeStoredNode(node) ?? defaults.node,
      sortField: isSessionSortField(sortField) ? sortField : defaults.sortField,
      sortOrder: isSortOrder(sortOrder) ? sortOrder : defaults.sortOrder,
      pageSize:
        typeof parsed.pageSize === 'number' &&
        Number.isInteger(parsed.pageSize) &&
        parsed.pageSize > 0 &&
        parsed.pageSize <= 500
          ? parsed.pageSize
          : defaults.pageSize,
    }
  } catch {
    return defaults
  }
}

export function saveSessionPrefs(prefs: SessionPrefs) {
  if (typeof window === 'undefined') return
  try {
    window.localStorage.setItem(PREFS_KEY, JSON.stringify(prefs))
  } catch {
    /* ignore */
  }
}

export function loadSessionTableColumnSettings() {
  if (typeof window === 'undefined') return coerceSessionTableColumnSettings(null)
  try {
    const raw = window.localStorage.getItem(SESSION_TABLE_COLUMN_STORAGE_KEY)
    if (!raw) return coerceSessionTableColumnSettings(null)
    return coerceSessionTableColumnSettings(JSON.parse(raw))
  } catch {
    return coerceSessionTableColumnSettings(null)
  }
}

export function saveSessionTableColumnSettings(settings: {
  sizes: SessionTableColumnSizes
  order: SessionTableColumnOrder
}) {
  if (typeof window === 'undefined') return
  try {
    window.localStorage.setItem(SESSION_TABLE_COLUMN_STORAGE_KEY, JSON.stringify(settings))
  } catch {
    /* ignore */
  }
}

export function sessionPageTitle(selectedNode: string | null): string {
  const normalized = normalizeStoredNode(selectedNode)
  if (!normalized || normalized.toLowerCase() === 'local') return ''
  return normalized
}

export function getErrorMessage(error: unknown, fallback: string): string {
  if (error instanceof Error) {
    const message = error.message.trim()
    return message === '' ? fallback : message
  }
  return fallback
}
