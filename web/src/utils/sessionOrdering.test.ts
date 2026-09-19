import { describe, expect, it } from 'vitest'
import { SessionSortField, SortOrder, type SessionSummary } from '@/api/types'
import {
  orderSessionPage,
  sessionIsActive,
  sessionIsPinnable,
  sessionPinKey,
} from './sessionOrdering'

function session(partial: Partial<SessionSummary>): SessionSummary {
  return {
    id: 'id',
    title: null,
    tags: [],
    command: 'cmd',
    args: [],
    pid: null,
    status: 'running',
    created_at: '2026-01-01T00:00:00Z',
    started_at: null,
    ended_at: null,
    cwd: null,
    input_needed: false,
    notifications_enabled: true,
    node: null,
    last_total_bytes: 0,
    last_output_epoch: null,
    ...partial,
  }
}

const defaultOrder = {
  pinnedKeys: [],
  node: null,
  sortField: SessionSortField.CreatedAt,
  sortOrder: SortOrder.Desc,
}

describe('sessionIsActive', () => {
  it('treats created, running and stopping as active', () => {
    expect(sessionIsActive(session({ status: 'created' }))).toBe(true)
    expect(sessionIsActive(session({ status: 'running' }))).toBe(true)
    expect(sessionIsActive(session({ status: 'stopping' }))).toBe(true)
  })

  it('treats finished sessions as inactive unless they wait for input', () => {
    expect(sessionIsActive(session({ status: 'stopped' }))).toBe(false)
    expect(sessionIsActive(session({ status: 'failed' }))).toBe(false)
    expect(sessionIsActive(session({ status: 'stopped', input_needed: true }))).toBe(true)
  })
})

describe('sessionIsPinnable / sessionPinKey', () => {
  it('only live sessions are pinnable', () => {
    expect(sessionIsPinnable(session({ status: 'created' }))).toBe(true)
    expect(sessionIsPinnable(session({ status: 'running' }))).toBe(true)
    expect(sessionIsPinnable(session({ status: 'stopping' }))).toBe(true)
    expect(sessionIsPinnable(session({ status: 'stopped' }))).toBe(false)
    expect(sessionIsPinnable(session({ status: 'failed' }))).toBe(false)
  })

  it('scopes pin keys to the node', () => {
    expect(sessionPinKey('abc')).toBe(':abc')
    expect(sessionPinKey('abc', 'worker-a')).toBe('worker-a:abc')
    expect(sessionPinKey('abc', ' worker-a ')).toBe('worker-a:abc')
  })
})

describe('orderSessionPage default sorting (active first)', () => {
  it('puts active sessions above finished ones, newest first', () => {
    const oldRunning = session({
      id: 'old-running',
      status: 'running',
      created_at: '2026-01-01T00:00:00Z',
    })
    const newStopped = session({
      id: 'new-stopped',
      status: 'stopped',
      created_at: '2026-01-03T00:00:00Z',
    })
    const oldFailed = session({
      id: 'old-failed',
      status: 'failed',
      created_at: '2025-12-31T00:00:00Z',
    })
    const newRunning = session({
      id: 'new-running',
      status: 'running',
      created_at: '2026-01-02T00:00:00Z',
    })

    const ordered = orderSessionPage([oldFailed, newStopped, oldRunning, newRunning], defaultOrder)
    expect(ordered.map((s) => s.id)).toEqual([
      'new-running',
      'old-running',
      'new-stopped',
      'old-failed',
    ])
  })

  it('counts an input-needed finished session as active (TUI parity)', () => {
    const waiting = session({
      id: 'waiting',
      status: 'stopped',
      input_needed: true,
      created_at: '2026-01-01T00:00:00Z',
    })
    const newerStopped = session({
      id: 'newer-stopped',
      status: 'stopped',
      created_at: '2026-01-02T00:00:00Z',
    })

    const ordered = orderSessionPage([newerStopped, waiting], defaultOrder)
    expect(ordered.map((s) => s.id)).toEqual(['waiting', 'newer-stopped'])
  })

  it('respects ascending order within the default sort', () => {
    const newer = session({ id: 'newer', status: 'running', created_at: '2026-01-02T00:00:00Z' })
    const older = session({ id: 'older', status: 'running', created_at: '2026-01-01T00:00:00Z' })
    const newerStopped = session({
      id: 'newer-stopped',
      status: 'stopped',
      created_at: '2026-01-03T00:00:00Z',
    })

    const ordered = orderSessionPage([newerStopped, newer, older], {
      ...defaultOrder,
      sortOrder: SortOrder.Asc,
    })
    expect(ordered.map((s) => s.id)).toEqual(['older', 'newer', 'newer-stopped'])
  })

  it('keeps the incoming order for a non-default sort field', () => {
    const stopped = session({ id: 'stopped', status: 'stopped' })
    const running = session({ id: 'running', status: 'running' })

    const ordered = orderSessionPage([stopped, running], {
      ...defaultOrder,
      sortField: SessionSortField.Title,
    })
    // Server order (title sort) is preserved untouched.
    expect(ordered.map((s) => s.id)).toEqual(['stopped', 'running'])
  })
})

describe('orderSessionPage pinning', () => {
  it('floats pinned sessions to the top, most recently pinned first', () => {
    const a = session({ id: 'a', status: 'running' })
    const b = session({ id: 'b', status: 'running' })
    const c = session({ id: 'c', status: 'running' })

    // c was pinned most recently (index 0), then a.
    const ordered = orderSessionPage([a, b, c], {
      ...defaultOrder,
      pinnedKeys: [':c', ':a'],
    })
    expect(ordered.map((s) => s.id)).toEqual(['c', 'a', 'b'])
  })

  it('pins rank above the active-first grouping', () => {
    const active = session({ id: 'active', status: 'running', created_at: '2026-01-02T00:00:00Z' })
    const pinnedRunning = session({
      id: 'pinned-running',
      status: 'running',
      created_at: '2026-01-01T00:00:00Z',
    })

    const ordered = orderSessionPage([active, pinnedRunning], {
      ...defaultOrder,
      pinnedKeys: [':pinned-running'],
    })
    expect(ordered.map((s) => s.id)).toEqual(['pinned-running', 'active'])
  })

  it('a pin no longer floats once the session is finished', () => {
    const stoppedPinned = session({
      id: 'pinned-stopped',
      status: 'stopped',
      created_at: '2026-01-01T00:00:00Z',
    })
    const running = session({
      id: 'running',
      status: 'running',
      created_at: '2026-01-02T00:00:00Z',
    })

    const ordered = orderSessionPage([stoppedPinned, running], {
      ...defaultOrder,
      pinnedKeys: [':pinned-stopped'],
    })
    // The stale pin is inert: normal active-first ordering applies.
    expect(ordered.map((s) => s.id)).toEqual(['running', 'pinned-stopped'])
  })

  it('scopes pins to the selected node', () => {
    const remoteSession = session({ id: 'same', status: 'running' })
    const otherRemote = session({
      id: 'other',
      status: 'running',
      created_at: '2026-01-02T00:00:00Z',
    })

    // Pinning the local list does not float the remote node's session...
    const localPinOnly = orderSessionPage([otherRemote, remoteSession], {
      ...defaultOrder,
      node: 'worker-a',
      pinnedKeys: [':same'],
    })
    expect(localPinOnly.map((s) => s.id)).toEqual(['other', 'same'])

    // ...while the node-scoped pin does.
    const remotePin = orderSessionPage([otherRemote, remoteSession], {
      ...defaultOrder,
      node: 'worker-a',
      pinnedKeys: ['worker-a:same'],
    })
    expect(remotePin.map((s) => s.id)).toEqual(['same', 'other'])
  })
})
