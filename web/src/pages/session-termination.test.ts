import { describe, expect, it } from 'vitest'
import type { SessionStatus, SessionSummary } from '@/api/types'
import {
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