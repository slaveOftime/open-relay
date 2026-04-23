import { useSyncExternalStore } from 'react'

import type { SessionEvent, SessionNotificationData, SessionSummary } from '@/api/types'
import { SparklineStore } from '@/components/SparklineSvg'

const sparklineStore = new SparklineStore()
const EMPTY_ACTIVITY_SERIES: number[] = []

export function useSessionActivitySeries(sessionId?: string | null): number[] {
  return useSyncExternalStore(
    (listener) => sparklineStore.subscribe(listener),
    () => (sessionId ? sparklineStore.getSeries(sessionId) : EMPTY_ACTIVITY_SERIES),
    () => EMPTY_ACTIVITY_SERIES
  )
}

export function recordSessionActivity(
  session: Pick<SessionSummary, 'id' | 'last_total_bytes'>
): void {
  sparklineStore.recordTotal(session.id, session.last_total_bytes)
}

export function recordSessionNotificationActivity(
  notification: Pick<SessionNotificationData, 'session_ids' | 'last_total_bytes'>
): void {
  notification.session_ids.forEach((sessionId) => {
    sparklineStore.recordTotal(sessionId, notification.last_total_bytes)
  })
}

export function removeSessionActivity(sessionId: string): void {
  sparklineStore.remove(sessionId)
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
      removeSessionActivity(event.data.id)
      return
    case 'session_notification':
      recordSessionNotificationActivity(event.data)
      return
  }
}
