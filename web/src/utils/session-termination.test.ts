import { describe, expect, it } from 'vitest'
import type { SessionStatus, SessionSummary } from '@/api/types'
import {
  applyPendingTermination,
  applyPendingTerminations,
  isTerminationSettled,
  PENDING_TERMINATION_STATUS,
  revertSessionStatus,
  withPendingTermination,
  withSessionStatus,
} from './session-termination'

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

describe('isTerminationSettled', () => {
  it('is false only while the session can still be terminated', () => {
    expect(isTerminationSettled('created')).toBe(false)
    expect(isTerminationSettled('running')).toBe(false)
  })

  it('is true for stopping and every finished status', () => {
    const settled: SessionStatus[] = ['stopping', 'stopped', 'killed', 'failed']
    for (const status of settled) {
      expect(isTerminationSettled(status)).toBe(true)
    }
  })
})

describe('withPendingTermination', () => {
  it('claims stopping without touching the rest of the summary', () => {
    const current = session({ title: 'demo', last_total_bytes: 120, input_needed: true })
    const pending = withPendingTermination(current)

    expect(pending).not.toBeNull()
    expect(pending?.status).toBe(PENDING_TERMINATION_STATUS)
    expect(pending).toEqual({ ...current, status: 'stopping' })
    // The captured summary must stay pristine so it can revert the write.
    expect(current.status).toBe('running')
  })

  it('declines once the session is already stopping or finished', () => {
    for (const status of ['stopping', 'stopped', 'killed', 'failed'] as const) {
      expect(withPendingTermination(session({ status }))).toBeNull()
    }
  })
})

describe('withSessionStatus', () => {
  it('patches the matching row and leaves the others untouched', () => {
    const items = [session({ id: 'a' }), session({ id: 'b' })]
    const next = withSessionStatus(items, 'a', 'stopping')

    expect(next).not.toBe(items)
    expect(next[0].status).toBe('stopping')
    expect(next[1]).toBe(items[1])
  })

  it('returns the same array when nothing would change', () => {
    const items = [session({ id: 'a' })]
    expect(withSessionStatus(items, 'a', 'running')).toBe(items)
    expect(withSessionStatus(items, 'missing', 'stopping')).toBe(items)
  })
})

describe('revertSessionStatus', () => {
  it('restores the previous status after a failed request', () => {
    const items = withSessionStatus([session({ id: 'a' })], 'a', 'stopping')
    const next = revertSessionStatus(items, 'a', 'stopping', 'running')

    expect(next[0].status).toBe('running')
  })

  it('never rolls back a server update that beat the failed request', () => {
    const items = withSessionStatus([session({ id: 'a' })], 'a', 'stopped')
    expect(revertSessionStatus(items, 'a', 'stopping', 'running')).toBe(items)
  })
})

describe('applyPendingTerminations', () => {
  it('keeps the optimistic status over a stale running snapshot', () => {
    const pending = new Map([['a', 'stopping' as SessionStatus]])
    const items = [session({ id: 'a', status: 'running' }), session({ id: 'b' })]

    const next = applyPendingTerminations(items, pending)

    expect(next[0].status).toBe('stopping')
    expect(next[1]).toBe(items[1])
    expect(pending.get('a')).toBe('stopping')
  })

  it('lets a progressed server status win and drops the pending entry', () => {
    const pending = new Map([['a', 'stopping' as SessionStatus]])
    const next = applyPendingTerminations([session({ id: 'a', status: 'stopped' })], pending)

    expect(next[0].status).toBe('stopped')
    expect(pending.has('a')).toBe(false)
  })

  it('returns the same array when nothing needs overriding', () => {
    const pending = new Map([['a', 'stopping' as SessionStatus]])
    const items = [session({ id: 'a', status: 'stopping' })]
    expect(applyPendingTerminations(items, pending)).toBe(items)
    expect(applyPendingTerminations([session()], new Map())).toHaveLength(1)
  })
})

describe('applyPendingTermination', () => {
  it('overrides a stale single-session event payload', () => {
    const pending = new Map([['a', 'stopping' as SessionStatus]])
    expect(applyPendingTermination(session({ id: 'a', status: 'running' }), pending).status).toBe(
      'stopping'
    )
    expect(applyPendingTermination(session({ id: 'b' }), pending).status).toBe('running')
  })

  it('accepts the authoritative status once it progressed', () => {
    const pending = new Map([['a', 'stopping' as SessionStatus]])
    expect(applyPendingTermination(session({ id: 'a', status: 'killed' }), pending).status).toBe(
      'killed'
    )
    expect(pending.has('a')).toBe(false)
  })
})
