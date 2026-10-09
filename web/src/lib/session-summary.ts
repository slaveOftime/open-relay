/**
 * Session identity + equality helpers for the shared session event store.
 *
 * Pure functions, no React and no store, so both the store and the hooks that
 * read it can import them without a cycle. Session ids can recur across
 * connected nodes, so every key is namespaced by node.
 */

import type { SessionSummary } from '@/api/types'

/** Normalize a node name to a stable `null`/string pair for key building. */
export function normalizeNode(node?: string | null): string | null {
  if (typeof node !== 'string') return null
  const trimmed = node.trim()
  return trimmed === '' ? null : trimmed
}

/** Session ids can recur across connected nodes; never mix them. */
export function sessionKey(id: string, node?: string | null): string {
  return `${normalizeNode(node) ?? ''}\0${id}`
}

function sameStringArray(left: string[], right: string[]): boolean {
  return left.length === right.length && left.every((value, index) => value === right[index])
}

function sameOptionalString(a: string | null | undefined, b: string | null | undefined): boolean {
  return (a ?? null) === (b ?? null)
}

/**
 * Field-by-field comparison so a re-rendered snapshot object that carries the
 * same values does not tear down every subscriber of the store.
 */
export function sameSessionSummary(a: SessionSummary, b: SessionSummary): boolean {
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
