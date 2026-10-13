import { subscribeEvents, type SseConnectionState } from '@/api/client'
import type { SessionEvent, SessionSummary } from '@/api/types'
import { ingestSessionActivityEvent, recordSessionActivity } from '@/lib/session-activity'
import { sameSessionSummary, sessionKey } from '@/lib/session-summary'

type StoreListener = () => void
type EventListener = (event: SessionEvent) => void

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

export const sessionEventsStore = new SessionEventsStore()

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
