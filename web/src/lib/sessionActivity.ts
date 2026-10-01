import type { SessionEvent, SessionNotificationData, SessionSummary } from '@/api/types'
import { SparklineStore, type SparklineActivitySnapshot } from '@/components/sparklineStore'

// Pin the singleton on globalThis so Vite HMR (or any future module-system
// replacement) keeps the same instance instead of rebuilding a fresh one and
// dropping totals / subscriptions on every dependency edit. This guarantees
// the SessionsPage row sparkline and the SessionDetailPage header sparkline
// share the same backing data across navigations and reloads.
declare global {
  var __olySparklineStore: SparklineStore | undefined
}

const sparklineStore: SparklineStore = globalThis.__olySparklineStore ?? new SparklineStore()
if (!globalThis.__olySparklineStore) globalThis.__olySparklineStore = sparklineStore
const EMPTY_ACTIVITY_SERIES: number[] = []
const EMPTY_ACTIVITY_SNAPSHOT: SparklineActivitySnapshot = {
  series: EMPTY_ACTIVITY_SERIES,
  bucketIndex: 0,
  lastOutputAt: null,
}

// Session ids can recur across connected nodes; never mix their byte totals.
export function sessionActivityKey(id: string, node?: string | null): string {
  return `${node?.trim() || ''}\0${id}`
}

export function getSessionActivitySnapshot(
  sessionId?: string | null,
  node?: string | null
): SparklineActivitySnapshot {
  return sessionId
    ? sparklineStore.getActivitySnapshot(sessionActivityKey(sessionId, node))
    : EMPTY_ACTIVITY_SNAPSHOT
}

export function subscribeSessionActivity(
  sessionId: string | null | undefined,
  node: string | null | undefined,
  listener: () => void
): () => void {
  return sessionId
    ? sparklineStore.subscribe(sessionActivityKey(sessionId, node), listener)
    : () => {}
}
export function recordSessionActivity(
  session: Pick<SessionSummary, 'id' | 'node' | 'last_total_bytes'>
): void {
  sparklineStore.recordTotal(sessionActivityKey(session.id, session.node), session.last_total_bytes)
}

export function recordSessionNotificationActivity(
  notification: Pick<SessionNotificationData, 'session_ids' | 'node' | 'last_total_bytes'>
): void {
  notification.session_ids.forEach((sessionId) => {
    sparklineStore.recordTotal(
      sessionActivityKey(sessionId, notification.node),
      notification.last_total_bytes
    )
  })
}

export function removeSessionActivity(sessionId: string, node?: string | null): void {
  sparklineStore.remove(sessionActivityKey(sessionId, node))
}

export function ingestSessionActivityEvent(event: SessionEvent): void {
  switch (event.event) {
    case 'snapshot':
      event.data.forEach(recordSessionActivity)
      return
    case 'session_created':
    case 'session_updated':
      recordSessionActivity(event.data)
      return
    case 'session_deleted':
      removeSessionActivity(event.data.id, event.data.node)
      return
    case 'session_notification':
      recordSessionNotificationActivity(event.data)
      return
  }
}
