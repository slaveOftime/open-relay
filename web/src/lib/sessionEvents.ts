import { useSyncExternalStore } from 'react'

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

function sameSessionSummary(left: SessionSummary, right: SessionSummary): boolean {
  return (
    left.id === right.id &&
    left.title === right.title &&
    sameStringArray(left.tags, right.tags) &&
    left.command === right.command &&
    sameStringArray(left.args, right.args) &&
    left.pid === right.pid &&
    left.status === right.status &&
    left.created_at === right.created_at &&
    left.started_at === right.started_at &&
    left.ended_at === right.ended_at &&
    left.cwd === right.cwd &&
    left.input_needed === right.input_needed &&
    left.notifications_enabled === right.notifications_enabled &&
    normalizeNode(left.node) === normalizeNode(right.node) &&
    left.last_total_bytes === right.last_total_bytes &&
    left.last_output_epoch === right.last_output_epoch
  )
}

class SessionEventsStore {
  private readonly storeListeners = new Set<StoreListener>()
  private readonly eventListeners = new Set<EventListener>()
  private readonly sessions = new Map<string, SessionSummary>()

  private connectionState: SseConnectionState =
    typeof navigator !== 'undefined' && !navigator.onLine ? 'offline' : 'reconnecting'
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
    this.setConnectionState(
      typeof navigator !== 'undefined' && !navigator.onLine ? 'offline' : 'reconnecting'
    )
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
      case 'session_notification':
        break
    }

    ingestSessionActivityEvent(event)
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

export function useSseConnectionState(): SseConnectionState {
  return useSyncExternalStore(
    (listener) => sessionEventsStore.subscribeStore(listener),
    () => sessionEventsStore.getConnectionState(),
    () => 'reconnecting'
  )
}

export function useLiveSessionSummary(
  id?: string | null,
  node?: string | null
): SessionSummary | null {
  return useSyncExternalStore(
    (listener) => sessionEventsStore.subscribeStore(listener),
    () => sessionEventsStore.getSession(id, node),
    () => null
  )
}
