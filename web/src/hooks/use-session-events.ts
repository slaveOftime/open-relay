import { useEffect, useSyncExternalStore } from 'react'

import type { SseConnectionState } from '@/api/client'
import type { SessionSummary } from '@/api/types'
import { sessionEventsStore, subscribeSessionEvents } from '@/lib/session-events'
import { normalizeNode } from '@/lib/session-summary'

// A stable subscribe function avoids tearing down and re-adding the store
// listener on every table render (e.g. when activity sparklines update).
const subscribeConnectionStore = (listener: () => void) =>
  sessionEventsStore.subscribeStore(listener)

export function useSseConnectionState(): SseConnectionState {
  return useSyncExternalStore(
    subscribeConnectionStore,
    () => sessionEventsStore.getConnectionState(),
    () => 'connecting'
  )
}

export function useLiveSessionSummary(
  id?: string | null,
  node?: string | null
): SessionSummary | null {
  return useSyncExternalStore(
    subscribeConnectionStore,
    () => sessionEventsStore.getSession(id, node),
    () => null
  )
}

/**
 * Reconcile handler. Fires once per matching SSE event for the duration
 * of the consumer's mount; consumers (e.g. SessionDetailPage) use this to
 * re-fetch the canonical session summary when the local store diverges
 * from the server's REST view.
 */
export function useReconcileTrigger(
  id?: string | null,
  node?: string | null,
  reconcile: ((reason: 'stream_ready' | 'resync_required') => void) | null = null
): void {
  useEffect(() => {
    if (!id || !reconcile) return
    return subscribeSessionEvents((event) => {
      if (event.event !== 'stream_ready' && event.event !== 'resync_required') return
      if (event.event === 'resync_required') {
        const target = normalizeNode(event.data.node)
        if (target !== null && target !== normalizeNode(node)) return
      }
      reconcile(event.event)
    })
  }, [id, node, reconcile])
}
