import { describe, expect, it } from 'vitest'

import type { SessionSummary } from '@/api/types'

import { normalizeNode, sameSessionSummary, sessionKey } from './session-summary'

/**
 * A fully populated summary, annotated with its type on purpose. `tsc` rejects a
 * missing property in a literal, so adding a field to `SessionSummary` fails to
 * compile until it is added here — and the "every field is compared" test below
 * then covers it automatically. Without that, a new field silently falls out of
 * `sameSessionSummary` and the store stops updating on it.
 */
const base: SessionSummary = {
  id: 'session-1',
  title: 'a title',
  tags: ['one', 'two'],
  command: 'bash',
  args: ['-l'],
  pid: 1234,
  status: 'running',
  created_at: '2026-01-01T00:00:00Z',
  started_at: '2026-01-01T00:00:01Z',
  ended_at: null,
  resume_command: 'oly attach session-1',
  cwd: '/tmp',
  input_needed: false,
  notifications_enabled: true,
  node: 'lab',
  last_total_bytes: 512,
  last_output_epoch: '2026-01-01T00:00:02Z',
  foreground_color: '#ff0000',
  background_color: '#00ff00',
}

/** A deep copy, so no test can pass by comparing a shared reference. */
const copy = (): SessionSummary => JSON.parse(JSON.stringify(base)) as SessionSummary

describe('normalizeNode', () => {
  it('collapses a missing, non-string or blank node to null', () => {
    expect(normalizeNode(undefined)).toBeNull()
    expect(normalizeNode(null)).toBeNull()
    expect(normalizeNode('')).toBeNull()
    expect(normalizeNode('   ')).toBeNull()
    expect(normalizeNode('\t\n')).toBeNull()
  })

  it('returns anything else trimmed', () => {
    expect(normalizeNode('lab')).toBe('lab')
    expect(normalizeNode('  lab  ')).toBe('lab')
  })
})

describe('sessionKey', () => {
  it('separates the same id on different nodes', () => {
    // Session ids recur across connected nodes, so an un-namespaced key would
    // let one node's row overwrite another's.
    expect(sessionKey('s1', 'lab')).not.toBe(sessionKey('s1', 'other'))
    expect(sessionKey('s1', 'lab')).not.toBe(sessionKey('s1'))
  })

  it('treats an absent and a blank node as the same scope', () => {
    expect(sessionKey('s1')).toBe(sessionKey('s1', null))
    expect(sessionKey('s1')).toBe(sessionKey('s1', '  '))
    expect(sessionKey('s1', ' lab ')).toBe(sessionKey('s1', 'lab'))
  })

  it('embeds the id so keys are distinguishable by id alone', () => {
    expect(sessionKey('a', 'lab')).not.toBe(sessionKey('b', 'lab'))
    expect(sessionKey('s1', 'lab').endsWith('s1')).toBe(true)
  })
})

describe('sameSessionSummary', () => {
  it('is true for the same object without walking fields', () => {
    expect(sameSessionSummary(base, base)).toBe(true)
  })

  it('is true for a deep-equal copy', () => {
    expect(sameSessionSummary(base, copy())).toBe(true)
  })

  it('is false for a different id even when everything else matches', () => {
    const other = { ...copy(), id: 'session-2' }

    expect(sameSessionSummary(base, other)).toBe(false)
  })

  it('detects a change in every declared field', () => {
    // One deliberately-wrong value per field. Driven off the type's own keys so a
    // field added to SessionSummary shows up here as an uncovered entry rather
    // than silently dropping out of the comparison.
    const mutations: Record<keyof SessionSummary, () => SessionSummary> = {
      id: () => ({ ...copy(), id: 'other' }),
      title: () => ({ ...copy(), title: 'other' }),
      tags: () => ({ ...copy(), tags: ['three'] }),
      command: () => ({ ...copy(), command: 'zsh' }),
      args: () => ({ ...copy(), args: ['-i'] }),
      pid: () => ({ ...copy(), pid: 9999 }),
      status: () => ({ ...copy(), status: 'stopped' }),
      created_at: () => ({ ...copy(), created_at: '2026-02-02T00:00:00Z' }),
      started_at: () => ({ ...copy(), started_at: '2026-02-02T00:00:00Z' }),
      ended_at: () => ({ ...copy(), ended_at: '2026-02-02T00:00:00Z' }),
      resume_command: () => ({ ...copy(), resume_command: 'something else' }),
      cwd: () => ({ ...copy(), cwd: '/var' }),
      input_needed: () => ({ ...copy(), input_needed: true }),
      notifications_enabled: () => ({ ...copy(), notifications_enabled: false }),
      node: () => ({ ...copy(), node: 'other' }),
      last_total_bytes: () => ({ ...copy(), last_total_bytes: 999 }),
      last_output_epoch: () => ({ ...copy(), last_output_epoch: '2026-03-03T00:00:00Z' }),
      foreground_color: () => ({ ...copy(), foreground_color: '#0000ff' }),
      background_color: () => ({ ...copy(), background_color: '#0000ff' }),
    }

    const uncovered = (Object.keys(base) as (keyof SessionSummary)[]).filter(
      (field) => !(field in mutations)
    )
    expect(uncovered).toEqual([])

    for (const [field, mutate] of Object.entries(mutations)) {
      const mutated = mutate()
      // The mutation must actually differ, or the test is vacuous.
      expect(
        mutated[field as keyof SessionSummary],
        `mutation for ${field} was a no-op`
      ).not.toEqual(base[field as keyof SessionSummary])
      expect(sameSessionSummary(base, mutate()), `a change to ${field} was not detected`).toBe(
        false
      )
    }
  })

  it('treats undefined and null as the same value', () => {
    // A server that omits an optional field and one that sends null describe the
    // same session; comparing them with === would tear down subscribers.
    expect(sameSessionSummary({ ...copy(), title: null }, { ...copy(), title: null })).toBe(true)
    // `node?` is the one field that may be absent entirely rather than null.
    expect(sameSessionSummary({ ...base, node: undefined }, { ...base, node: null })).toBe(true)
  })

  it('does not treat an empty string as absent', () => {
    expect(sameSessionSummary(base, { ...copy(), title: '' })).toBe(false)
    expect(sameSessionSummary({ ...base, title: '' }, { ...base, title: null })).toBe(false)
  })

  it('normalizes the node before comparing', () => {
    // 'lab' and ' lab ' are the same node; treating them as different would
    // rewrite the store entry every time a remote echoes it with whitespace.
    expect(sameSessionSummary(base, { ...copy(), node: ' lab ' })).toBe(true)
    expect(sameSessionSummary(base, { ...copy(), node: '' })).toBe(
      sameSessionSummary(base, { ...base, node: undefined })
    )
  })

  it('compares tags and args by content and order', () => {
    expect(sameSessionSummary(base, { ...copy(), tags: ['two', 'one'] })).toBe(false)
    expect(sameSessionSummary(base, { ...copy(), tags: ['one'] })).toBe(false)
    expect(sameSessionSummary(base, { ...copy(), tags: ['one', 'two'] })).toBe(true)
    expect(sameSessionSummary(base, { ...copy(), args: [] })).toBe(false)
  })

  it('treats an absent node and an explicit null node as the same', () => {
    // `node?` is optional in the Rust protocol, so a summary may omit it.
    expect(sameSessionSummary({ ...base, node: undefined }, { ...base, node: null })).toBe(true)
  })
})
