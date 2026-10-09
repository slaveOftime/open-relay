/**
 * Session tag normalization.
 *
 * Tags arrive from the API, from stored preferences and from the filter
 * input, so they are normalized in one place: trimmed, empty entries
 * dropped and duplicates removed in first-seen order. Used by the tag list
 * and by grouping/filtering.
 */

import type { SessionSummary } from '@/api/types'

export function normalizeSessionTags(tags: SessionSummary['tags']): string[] {
  const seen = new Set<string>()
  const out: string[] = []
  for (const raw of Array.isArray(tags) ? tags : []) {
    const tag = typeof raw === 'string' ? raw.trim() : ''
    if (!tag || seen.has(tag)) continue
    seen.add(tag)
    out.push(tag)
  }
  return out
}
