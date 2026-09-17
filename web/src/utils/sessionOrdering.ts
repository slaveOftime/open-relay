import { SessionSortField, SortOrder } from '@/api/types'
import type { SessionSummary } from '@/api/types'

// ---------------------------------------------------------------------------
// Session list ordering (mirrors the TUI's default sort in list_tui.rs)
// ---------------------------------------------------------------------------

/** Statuses that keep a session "active" (alive / not finished). */
export function isActiveSessionStatus(status: SessionSummary['status']): boolean {
  return status === 'created' || status === 'running' || status === 'stopping'
}

/**
 * A session is "active" for default ordering while it is alive (created/
 * running/stopping) or waiting for input, so attention-needed rows never
 * sink below finished ones. Same rule as `session_is_active` in the TUI.
 */
export function sessionIsActive(session: SessionSummary): boolean {
  return isActiveSessionStatus(session.status) || session.input_needed
}

/** A session can be pinned while it is live (not yet finished). */
export function sessionIsPinnable(session: SessionSummary): boolean {
  return isActiveSessionStatus(session.status)
}

/** Stable identity for a pin, scoped to the node the list is showing. */
export function sessionPinKey(sessionId: string, node?: string | null): string {
  const normalizedNode = typeof node === 'string' ? node.trim() : ''
  return `${normalizedNode}:${sessionId}`
}

function pinnedRankOf(
  session: SessionSummary,
  node: string | null,
  ranks: Map<string, number>
): number {
  const rank = ranks.get(sessionPinKey(session.id, node))
  // A pin only floats a session while it is still live; a finished pinned
  // session drops back to its regular position instead of squatting at the
  // top of the list.
  if (rank === undefined || !sessionIsPinnable(session)) return Number.MAX_SAFE_INTEGER
  return rank
}

export type SessionOrderOptions = {
  /** Pinned session keys, most recently pinned first. */
  pinnedKeys: readonly string[]
  /** Node scope used to resolve pin keys (`null` = local). */
  node?: string | null
  /** Active-first grouping is applied when the current sort field is the
   * default (Created At); an explicit user sort column wins instead. */
  sortField: SessionSortField
  sortOrder: SortOrder
}

/**
 * Order a loaded session page for display:
 *
 * 1. Pinned live sessions first, most recently pinned at the very top.
 * 2. When sorting by the default (Created At), active sessions next —
 *    alive or waiting for input — like the TUI's "active first" strategy.
 * 3. Everything else, keeping the sort the server applied.
 *
 * The sort is stable, so when neither (1) nor (2) reorders a pair, the
 * incoming (server) order is preserved untouched.
 */
export function orderSessionPage(
  sessions: readonly SessionSummary[],
  { pinnedKeys, node = null, sortField, sortOrder }: SessionOrderOptions
): SessionSummary[] {
  const ranks = new Map(pinnedKeys.map((key, index) => [key, index]))
  const activeFirst = sortField === SessionSortField.CreatedAt
  const createdSign = sortOrder === SortOrder.Asc ? 1 : -1

  return [...sessions].sort((a, b) => {
    const pinDiff =
      pinnedRankOf(a, node, ranks) - pinnedRankOf(b, node, ranks)
    if (pinDiff !== 0) return pinDiff

    if (activeFirst) {
      const activeDiff = Number(sessionIsActive(b)) - Number(sessionIsActive(a))
      if (activeDiff !== 0) return activeDiff

      const createdDiff =
        Date.parse(a.created_at) * createdSign - Date.parse(b.created_at) * createdSign
      if (createdDiff !== 0) return createdDiff
    }
    return 0
  })
}

// ---------------------------------------------------------------------------
// Pinned sessions — persisted in browser local storage only (never sent to
// the daemon). Keys are stored most-recently-pinned first.
// ---------------------------------------------------------------------------

export const SESSION_PINNED_STORAGE_KEY = 'open-relay.webv2.sessions.pinned.v1'

export function loadPinnedSessionKeys(): string[] {
  if (typeof window === 'undefined') return []
  try {
    const raw = window.localStorage.getItem(SESSION_PINNED_STORAGE_KEY)
    if (!raw) return []
    const parsed = JSON.parse(raw) as unknown
    if (!Array.isArray(parsed)) return []
    const seen = new Set<string>()
    const keys: string[] = []
    for (const entry of parsed) {
      if (typeof entry !== 'string' || entry.length === 0) continue
      if (seen.has(entry)) continue
      seen.add(entry)
      keys.push(entry)
    }
    return keys
  } catch {
    return []
  }
}

export function savePinnedSessionKeys(keys: readonly string[]): void {
  if (typeof window === 'undefined') return
  try {
    window.localStorage.setItem(SESSION_PINNED_STORAGE_KEY, JSON.stringify(keys))
  } catch {
    /* ignore */
  }
}
