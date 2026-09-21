import { describe, expect, it, vi } from 'vitest'
import type { SessionEvent, SessionSummary } from '@/api/types'
import { sessionPinKey } from '@/utils/sessionOrdering'
import {
  handleSessionPageEvent,
  normalizeStoredNode,
  type SessionPageEventContext,
  type SessionPageEventHandlers,
} from './sessions-page-events'

function session(partial: Partial<SessionSummary> = {}): SessionSummary {
  return {
    id: 'sess-1',
    title: null,
    tags: [],
    command: 'pi',
    args: [],
    pid: 42,
    status: 'running',
    created_at: '2026-01-01T00:00:00Z',
    started_at: null,
    ended_at: null,
    cwd: null,
    input_needed: false,
    notifications_enabled: false,
    node: null,
    last_total_bytes: 0,
    last_output_epoch: null,
    ...partial,
  }
}

function setup(overrides: Partial<SessionPageEventContext> = {}) {
  const context: SessionPageEventContext = {
    selectedNode: null,
    statusFilter: 'all',
    loadedSessionIds: new Set(['sess-1']),
    pushSubscribed: false,
    ...overrides,
  }
  const handlers: SessionPageEventHandlers = {
    applySnapshot: vi.fn(),
    replaceLoadedSession: vi.fn(),
    removeLoadedSession: vi.fn(),
    removePinnedKey: vi.fn(),
    reloadSessions: vi.fn(),
    scheduleDelayedReload: vi.fn(),
    showNotification: vi.fn(),
  }
  return { context, handlers }
}

describe('normalizeStoredNode', () => {
  it('treats missing, blank and non-string nodes as local', () => {
    expect(normalizeStoredNode(undefined)).toBeNull()
    expect(normalizeStoredNode(null)).toBeNull()
    expect(normalizeStoredNode(7)).toBeNull()
    expect(normalizeStoredNode('   ')).toBeNull()
    expect(normalizeStoredNode(' worker-a ')).toBe('worker-a')
  })
})

describe('handleSessionPageEvent', () => {
  it('applies snapshots on the local page only', () => {
    const local = setup()
    const items = [session()]
    handleSessionPageEvent({ event: 'snapshot', data: items }, local.context, local.handlers)
    expect(local.handlers.applySnapshot).toHaveBeenCalledWith(items)

    const remote = setup({ selectedNode: 'worker-a' })
    handleSessionPageEvent({ event: 'snapshot', data: items }, remote.context, remote.handlers)
    expect(remote.handlers.applySnapshot).not.toHaveBeenCalled()
  })

  it('ignores events from other nodes', () => {
    const { context, handlers } = setup({ selectedNode: 'worker-a' })
    const event: SessionEvent = { event: 'session_created', data: session({ node: 'worker-b' }) }
    handleSessionPageEvent(event, context, handlers)
    expect(handlers.reloadSessions).not.toHaveBeenCalled()
  })

  it('reloads in the background when a session appears on this node', () => {
    const { context, handlers } = setup()
    handleSessionPageEvent(
      { event: 'session_created', data: session({ node: undefined }) },
      context,
      handlers
    )
    expect(handlers.reloadSessions).toHaveBeenCalledWith({ background: true })
  })

  it('replaces a loaded session in place', () => {
    const { context, handlers } = setup()
    const updated = session({ status: 'stopped' })
    handleSessionPageEvent({ event: 'session_updated', data: updated }, context, handlers)
    expect(handlers.replaceLoadedSession).toHaveBeenCalledWith(updated)
    expect(handlers.scheduleDelayedReload).not.toHaveBeenCalled()
  })

  it('drops a session that no longer matches the status filter and reloads', () => {
    const { context, handlers } = setup({ statusFilter: 'running' })
    const updated = session({ status: 'stopped' })
    handleSessionPageEvent({ event: 'session_updated', data: updated }, context, handlers)
    expect(handlers.removeLoadedSession).toHaveBeenCalledWith('sess-1')
    expect(handlers.reloadSessions).toHaveBeenCalledWith({ background: true })
    expect(handlers.replaceLoadedSession).not.toHaveBeenCalled()
  })

  it('defers to a delayed reload for sessions the page has not loaded', () => {
    const { context, handlers } = setup({ loadedSessionIds: new Set() })
    handleSessionPageEvent({ event: 'session_updated', data: session() }, context, handlers)
    expect(handlers.scheduleDelayedReload).toHaveBeenCalledOnce()
    expect(handlers.replaceLoadedSession).not.toHaveBeenCalled()
  })

  it('drops deleted sessions and their pins', () => {
    const { context, handlers } = setup()
    handleSessionPageEvent(
      { event: 'session_deleted', data: { id: 'sess-1', node: 'worker-a' } },
      { ...context, selectedNode: 'worker-a' },
      handlers
    )
    expect(handlers.removeLoadedSession).toHaveBeenCalledWith('sess-1')
    expect(handlers.removePinnedKey).toHaveBeenCalledWith(sessionPinKey('sess-1', 'worker-a'))
    expect(handlers.reloadSessions).toHaveBeenCalledWith({ background: true })
  })

  it('leaves notifications to push when subscribed', () => {
    const notification: SessionEvent = {
      event: 'session_notification',
      data: {
        kind: 'input_needed',
        title: 'Input required',
        description: 'Waiting',
        body: 'Password:',
        session_ids: ['sess-1'],
        node: null,
        last_total_bytes: 0,
      },
    }

    const local = setup()
    handleSessionPageEvent(notification, local.context, local.handlers)
    expect(local.handlers.showNotification).toHaveBeenCalledOnce()

    const pushed = setup({ pushSubscribed: true })
    handleSessionPageEvent(notification, pushed.context, pushed.handlers)
    expect(pushed.handlers.showNotification).not.toHaveBeenCalled()
  })
})
