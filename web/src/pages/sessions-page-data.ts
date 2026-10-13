/**
 * Session-list fetching: request de-duplication and the params the list,
 * the remote-node pull and the filter reload all build identically.
 */

import type { ListParams } from '@/api/client'
import { fetchSessions } from '@/api/client'
import type { SessionSummary } from '@/api/types'

/**
 * In-flight requests keyed by their params. Two callers asking for the same
 * page of the same list share one HTTP request instead of racing; the entry
 * is dropped when it settles so a later repeat is a fresh fetch rather than a
 * cached answer.
 */
const sessionPageRequests = new Map<string, Promise<{ items: SessionSummary[]; total: number }>>()

export type SessionListQuery = {
  search: string
  statusFilter: 'all' | SessionSummary['status']
  page: number
  pageSize: number
  sortField: ListParams['sort']
  sortOrder: ListParams['order']
  node?: string | null
}

/**
 * The query the list is actually shown for. `loadLocal`, `loadRemote` and the
 * remote-node pull each built this by hand before, and they had to agree.
 */
export function buildSessionListParams(query: SessionListQuery): ListParams {
  return {
    search: query.search || undefined,
    status: query.statusFilter === 'all' ? undefined : query.statusFilter,
    limit: query.pageSize,
    offset: query.page * query.pageSize,
    sort: query.sortField,
    order: query.sortOrder,
    ...(query.node ? { node: query.node } : {}),
  }
}

export function getSessionListRequestKey(params: ListParams): string {
  return JSON.stringify({
    search: params.search ?? '',
    status: params.status ?? '',
    limit: params.limit ?? null,
    offset: params.offset ?? null,
    sort: params.sort ?? '',
    order: params.order ?? '',
    node: params.node ?? '',
  })
}

export function fetchSessionsOnce(params: ListParams) {
  const key = getSessionListRequestKey(params)
  const existing = sessionPageRequests.get(key)
  if (existing) return existing

  const request = fetchSessions(params).finally(() => {
    if (sessionPageRequests.get(key) === request) {
      sessionPageRequests.delete(key)
    }
  })
  sessionPageRequests.set(key, request)
  return request
}
