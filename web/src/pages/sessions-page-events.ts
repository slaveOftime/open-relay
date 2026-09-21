import type {
  SessionEvent,
  SessionNotificationData,
  SessionStatusFilter,
  SessionSummary,
} from '@/api/types'
import { sessionPinKey } from '@/utils/sessionOrdering'

/**
 * Sessions-page handling of the shared SSE stream.
 *
 * The page keeps exactly one subscription for its whole lifetime and reads the
 * current filter/node state through a ref, so typing in the search box or
 * turning a page never tears the event stream down (see `SessionsPage`).
 * Keeping the branch logic here makes it testable without React.
 */

/** Turn loose node values (URL params, prefs, events) into the canonical `null`. */
export function normalizeStoredNode(value: unknown): string | null {
  if (typeof value !== 'string') return null
  const trimmed = value.trim()
  return trimmed === '' ? null : trimmed
}

export function matchesSelectedNode(
  selectedNode: string | null,
  eventNode: string | null | undefined
): boolean {
  return (selectedNode ?? null) === normalizeStoredNode(eventNode)
}

export function matchesStatusFilter(
  statusFilter: SessionStatusFilter,
  status: SessionSummary['status']
): boolean {
  return statusFilter === 'all' || status === statusFilter
}

/** State the handlers read at event time, never from a captured render closure. */
export type SessionPageEventContext = {
  selectedNode: string | null
  statusFilter: SessionStatusFilter
  /** Ids currently rendered on the page. */
  loadedSessionIds: ReadonlySet<string>
  /** True while push notifications own the notification channel. */
  pushSubscribed: boolean
}

export type SessionPageEventHandlers = {
  applySnapshot: (items: SessionSummary[]) => void
  replaceLoadedSession: (session: SessionSummary) => void
  removeLoadedSession: (sessionId: string) => void
  removePinnedKey: (pinKey: string) => void
  reloadSessions: (opts?: { background?: boolean }) => void
  scheduleDelayedReload: () => void
  showNotification: (data: SessionNotificationData) => void
}

export function handleSessionPageEvent(
  event: SessionEvent,
  context: SessionPageEventContext,
  handlers: SessionPageEventHandlers
): void {
  switch (event.event) {
    case 'snapshot': {
      // Snapshots describe local sessions only.
      if (context.selectedNode) return
      handlers.applySnapshot(event.data)
      return
    }
    case 'session_created': {
      if (!matchesSelectedNode(context.selectedNode, event.data.node)) return
      handlers.reloadSessions({ background: true })
      return
    }
    case 'session_updated': {
      if (!matchesSelectedNode(context.selectedNode, event.data.node)) return
      // The session left the active page (filter changed underneath it): drop it
      // and let a background reload fix paging and totals.
      if (!matchesStatusFilter(context.statusFilter, event.data.status)) {
        handlers.removeLoadedSession(event.data.id)
        handlers.reloadSessions({ background: true })
        return
      }
      // A session we have never loaded belongs to another page/sort window.
      if (!context.loadedSessionIds.has(event.data.id)) {
        handlers.scheduleDelayedReload()
        return
      }
      handlers.replaceLoadedSession(event.data)
      return
    }
    case 'session_deleted': {
      if (!matchesSelectedNode(context.selectedNode, event.data.node)) return
      handlers.removeLoadedSession(event.data.id)
      handlers.removePinnedKey(sessionPinKey(event.data.id, normalizeStoredNode(event.data.node)))
      handlers.reloadSessions({ background: true })
      return
    }
    case 'session_notification': {
      if (context.pushSubscribed) return
      handlers.showNotification(event.data)
      return
    }
  }
}
