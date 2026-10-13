import { describe, expect, it } from 'vitest'

import type { SessionSummary } from '@/api/types'
import {
  didSessionVisibleOutputAdvance,
  isSessionRunning,
  normalizeSnapshotOutputForXterm,
} from './session-detail-output'

function summary(overrides: Partial<SessionSummary> = {}): SessionSummary {
  return {
    id: 'session-1',
    command: 'zsh',
    args: [],
    status: 'running',
    created_at: 0,
    last_total_bytes: 0,
    ...overrides,
  } as SessionSummary
}

describe('normalizeSnapshotOutputForXterm', () => {
  it('inserts CR before every bare LF', () => {
    const input = new Uint8Array([0x61, 0x0a, 0x62, 0x0a])

    expect(Array.from(normalizeSnapshotOutputForXterm(input))).toEqual([
      0x61, 0x0d, 0x0a, 0x62, 0x0d, 0x0a,
    ])
  })

  it('leaves existing CRLF pairs untouched and returns the input', () => {
    const input = new Uint8Array([0x0d, 0x0a, 0x41])

    const out = normalizeSnapshotOutputForXterm(input)

    expect(Array.from(out)).toEqual([0x0d, 0x0a, 0x41])
    expect(out).toBe(input)
  })

  it('handles a leading newline with no previous byte', () => {
    const out = normalizeSnapshotOutputForXterm(new Uint8Array([0x0a]))

    expect(Array.from(out)).toEqual([0x0d, 0x0a])
  })

  it('does not double up on a LF that already follows a CR', () => {
    const out = normalizeSnapshotOutputForXterm(new Uint8Array([0x0d, 0x0d, 0x0a]))

    // Only the second CR is adjacent to the LF, so the first LF stays bare.
    expect(Array.from(out)).toEqual([0x0d, 0x0d, 0x0a])
  })

  it('passes through plain text unchanged', () => {
    const input = new Uint8Array([0x68, 0x69, 0x0d])

    expect(normalizeSnapshotOutputForXterm(input)).toBe(input)
  })
})

describe('didSessionVisibleOutputAdvance', () => {
  it('treats a first sighting with any output as an advance', () => {
    expect(didSessionVisibleOutputAdvance(null, summary({ last_total_bytes: 1 }))).toBe(true)
  })

  it('does not pretend a brand-new silent session advanced', () => {
    expect(didSessionVisibleOutputAdvance(null, summary())).toBe(false)
  })

  it('advances when the epoch changes even at the same byte count', () => {
    const before = summary({ last_output_epoch: '1', last_total_bytes: 100 })
    const after = summary({ last_output_epoch: '2', last_total_bytes: 100 })

    expect(didSessionVisibleOutputAdvance(before, after)).toBe(true)
  })

  it('advances when bytes grow and the epoch is absent', () => {
    const before = summary({ last_total_bytes: 100 })
    const after = summary({ last_total_bytes: 101 })

    expect(didSessionVisibleOutputAdvance(before, after)).toBe(true)
  })

  it('does not advance on unrelated metadata changes', () => {
    const before = summary({ last_total_bytes: 100, title: 'one' })
    const after = summary({ last_total_bytes: 100, title: 'two' })

    expect(didSessionVisibleOutputAdvance(before, after)).toBe(false)
  })
})

describe('isSessionRunning', () => {
  it('counts running, stopping and created as live', () => {
    expect(isSessionRunning(summary({ status: 'running' }))).toBe(true)
    expect(isSessionRunning(summary({ status: 'stopping' }))).toBe(true)
    expect(isSessionRunning(summary({ status: 'created' }))).toBe(true)
  })

  it('treats stopped, failed and killed as not live', () => {
    expect(isSessionRunning(summary({ status: 'stopped' }))).toBe(false)
    expect(isSessionRunning(summary({ status: 'failed' }))).toBe(false)
    expect(isSessionRunning(summary({ status: 'killed' }))).toBe(false)
  })

  it('returns false with no session at all', () => {
    expect(isSessionRunning(null)).toBe(false)
  })
})
