import { useCallback, useSyncExternalStore } from 'react'

import type { SessionEvent, SessionNotificationData, SessionSummary } from '@/api/types'
import { SparklineStore } from '@/components/sparklineStore'

const sparklineStore = new SparklineStore()
const EMPTY_ACTIVITY_SERIES: number[] = []

// Session ids can recur across connected nodes; never mix their byte totals.
export function sessionActivityKey(id: string, node?: string | null): string {
  return `${node?.trim() || ''}\0${id}`
}

export function useSessionActivitySeries(sessionId?: string | null, node?: string | null): number[] {
  const key = sessionId ? sessionActivityKey(sessionId, node) : null
  // Keep the subscription stable across row renders and listen only to this
  // session; an update on another node/row must not wake every sparkline.
  const subscribe = useCallback(
    (listener: () => void) =>
      key ? sparklineStore.subscribe(key, listener) : () => {},
    [key]
  )
  const getSnapshot = useCallback(
    () => (key ? sparklineStore.getSeries(key) : EMPTY_ACTIVITY_SERIES),
    [key]
  )
  return useSyncExternalStore(subscribe, getSnapshot, () => EMPTY_ACTIVITY_SERIES)
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
