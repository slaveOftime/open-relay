import { useEffect, useSyncExternalStore } from 'react'

import { subscribeEvents, type SseConnectionState } from '@/api/client'
import type { SessionEvent, SessionSummary } from '@/api/types'
import { ingestSessionActivityEvent, recordSessionActivity } from '@/lib/sessionActivity'

type StoreListener = () => void
type EventListener = (event: SessionEvent) => void

function normalizeNode(node?: string | null): string | null {
  if (typeof node !== 'string') return null
  const trimmed = node.trim()
  return trimmed === '' ? null : trimmed
}

function sessionKey(id: string, node?: string | null): string {
  return `${normalizeNode(node) ?? ''}\0${id}`
}

function sameStringArray(left: string[], right: string[]): boolean {
  return left.length === right.length && left.every((value, index) => value === right[index])
}

function sameOptionalString(a: string | null | undefined, b: string | null | undefined): boolean {
  return (a ?? null) === (b ?? null)
}

function sameSessionSummary(a: SessionSummary, b: SessionSummary): boolean {
  if (a === b) return true
  if (a.id !== b.id) return false
  if (!sameOptionalString(a.title, b.title)) return false
  if (!sameStringArray(a.tags, b.tags)) return false
  if (a.command !== b.command) return false
  if (!sameStringArray(a.args, b.args)) return false
  if ((a.pid ?? null) !== (b.pid ?? null)) return false
  if (a.status !== b.status) return false
  if (a.created_at !== b.created_at) return false
  if (!sameOptionalString(a.started_at, b.started_at)) return false
  if (!sameOptionalString(a.ended_at, b.ended_at)) return false
  if (!sameOptionalString(a.resume_command ?? null, b.resume_command ?? null)) return false
  if (!sameOptionalString(a.cwd, b.cwd)) return false
  if (a.input_needed !== b.input_needed) return false
  if (a.notifications_enabled !== b.notifications_enabled) return false
  if (normalizeNode(a.node) !== normalizeNode(b.node)) return false
  if (a.last_total_bytes !== b.last_total_bytes) return false
  if (!sameOptionalString(a.last_output_epoch ?? null, b.last_output_epoch ?? null)) return false
  if (!sameOptionalString(a.foreground_color ?? null, b.foreground_color ?? null)) return false
  if (!sameOptionalString(a.background_color ?? null, b.background_color ?? null)) return false
  return true
}

class SessionEventsStore {
  private readonly storeListeners = new Set<StoreListener>()
  private readonly eventListeners = new Set<EventListener>()
  private readonly sessions = new Map<string, SessionSummary>()

  /** State to show while no stream is retained: never-connected reads as `connecting`. */
  private static idleState(): SseConnectionState {
    return typeof navigator !== 'undefined' && !navigator.onLine ? 'offline' : 'connecting'
  }

  private connectionState: SseConnectionState = SessionEventsStore.idleState()
  private cleanup: (() => void) | null = null
  private startRaf: number | null = null
  private retainCount = 0

  retain(): void {
    this.retainCount += 1
    if (this.cleanup || this.startRaf !== null || typeof window === 'undefined') return

    this.startRaf = window.requestAnimationFrame(() => {
      this.startRaf = null
      if (this.retainCount === 0 || this.cleanup) return
      this.cleanup = subscribeEvents(
        (event) => this.handleEvent(event),
        (state) => this.setConnectionState(state)
      )
    })
  }

  release(): void {
    this.retainCount = Math.max(0, this.retainCount - 1)
    if (this.retainCount > 0) return

    if (this.startRaf !== null && typeof window !== 'undefined') {
      window.cancelAnimationFrame(this.startRaf)
      this.startRaf = null
    }
    if (this.cleanup) {
      this.cleanup()
      this.cleanup = null
    }
    this.setConnectionState(SessionEventsStore.idleState())
  }

  subscribeStore(listener: StoreListener): () => void {
    this.retain()
    this.storeListeners.add(listener)
    return () => {
      this.storeListeners.delete(listener)
      this.release()
    }
  }

  subscribeEvents(listener: EventListener): () => void {
    this.retain()
    this.eventListeners.add(listener)
    return () => {
      this.eventListeners.delete(listener)
      this.release()
    }
  }

  getConnectionState(): SseConnectionState {
    return this.connectionState
  }

  getSession(id?: string | null, node?: string | null): SessionSummary | null {
    if (!id) return null
    return this.sessions.get(sessionKey(id, node)) ?? null
  }

  seedSession(session: SessionSummary): void {
    if (this.upsertSession(session)) {
      this.emitStore()
    }
  }

  seedSessions(items: SessionSummary[]): void {
    let changed = false
    for (const session of items) {
      changed = this.upsertSession(session) || changed
    }
    if (changed) {
      this.emitStore()
    }
  }

  private emitStore(): void {
    this.storeListeners.forEach((listener) => listener())
  }

  private setConnectionState(state: SseConnectionState): void {
    if (this.connectionState === state) return
    this.connectionState = state
    this.emitStore()
  }

  private handleEvent(event: SessionEvent): void {
    let changed = false

    switch (event.event) {
      case 'snapshot':
        changed = this.replaceLocalSnapshot(event.data)
        break
      case 'session_created':
      case 'session_updated':
        changed = this.upsertSession(event.data)
        break
      case 'session_deleted':
        changed = this.sessions.delete(sessionKey(event.data.id, event.data.node))
        break
      case 'stream_ready':
        // No payload to record yet; reconciliation happens separately.
        break
      case 'session_notification':
        break
    }

    // Activity and reconciliation are the responsibility of the activity store.
    if (
      event.event === 'session_deleted' ||
      event.event === 'session_notification' ||
      event.event === 'session_activity' ||
      event.event === 'resync_required' ||
      event.event === 'stream_ready' ||
      event.event === 'node_state'
    ) {
      ingestSessionActivityEvent(event)
    }
    this.eventListeners.forEach((listener) => listener(event))
    if (changed) {
      this.emitStore()
    }
  }

  private replaceLocalSnapshot(items: SessionSummary[]): boolean {
    let changed = false
    for (const key of Array.from(this.sessions.keys())) {
      if (!key.startsWith('\0')) continue
      changed = this.sessions.delete(key) || changed
    }
    for (const session of items) {
      changed = this.upsertSession(session) || changed
    }
    return changed
  }

  private upsertSession(session: SessionSummary): boolean {
    recordSessionActivity(session)
    const key = sessionKey(session.id, session.node)
    const current = this.sessions.get(key)
    if (current && sameSessionSummary(current, session)) {
      return false
    }
    this.sessions.set(key, session)
    return true
  }
}

const sessionEventsStore = new SessionEventsStore()

export function startSessionEvents(): void {
  sessionEventsStore.retain()
}

export function stopSessionEvents(): void {
  sessionEventsStore.release()
}

export function subscribeSessionEvents(listener: EventListener): () => void {
  return sessionEventsStore.subscribeEvents(listener)
}

export function ingestSessionSummary(session: SessionSummary): void {
  sessionEventsStore.seedSession(session)
}

export function ingestSessionSummaries(items: SessionSummary[]): void {
  sessionEventsStore.seedSessions(items)
}

// A stable subscribe function avoids tearing down and re-adding the store
// listener on every table render (e.g. when activity sparklines update).
const subscribeConnectionStore = (listener: StoreListener) =>
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
    return sessionEventsStore.subscribeEvents((event) => {
      if (event.event !== 'stream_ready' && event.event !== 'resync_required') return
      if (event.event === 'resync_required') {
        const target = normalizeNode(event.data.node)
        if (target !== null && target !== normalizeNode(node)) return
      }
      reconcile(event.event)
    })
  }, [id, node, reconcile])
}
