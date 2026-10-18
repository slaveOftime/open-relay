/**
 * How the sessions list is grouped. Shared by the page (which groups and
 * sorts) and the group header (which renders the label), so it lives on its
 * own rather than in either.
 */

export type GroupBy = 'none' | 'cwd' | 'command' | 'tag'

/**
 * The identity a collapse is remembered under.
 *
 * A label only means something inside the grouping that produced it: a
 * command named "bash" and a tag named "bash" are different groups that
 * happen to share a label. Including the grouping keeps a collapsed command
 * group from swallowing an unrelated tag group — or vice versa — when the
 * user switches how the list is grouped.
 */
export function collapsedGroupKey(groupBy: GroupBy, keyLabel: string): string {
  return `${groupBy}\u0000${keyLabel}`
}

/**
 * Where the collapse state is remembered, so a reload or a round trip into a
 * session detail page comes back to the same groups open or closed.
 */
export const COLLAPSED_GROUPS_STORAGE_KEY = 'open-relay.webv2.sessions.collapsed-groups.v1'

/**
 * The keys of the currently collapsed groups. Everything that is not a
 * non-empty string is dropped rather than trusted, and a missing or corrupt
 * entry degrades to "nothing collapsed" — losing the state must not stop the
 * list from rendering.
 */
export function loadCollapsedGroupKeys(): string[] {
  if (typeof window === 'undefined') return []
  try {
    const raw = window.localStorage.getItem(COLLAPSED_GROUPS_STORAGE_KEY)
    if (!raw) return []
    const parsed = JSON.parse(raw) as unknown
    if (!Array.isArray(parsed)) return []
    const seen = new Set<string>()
    const keys: string[] = []
    for (const entry of parsed) {
      if (typeof entry !== 'string' || entry.length === 0 || seen.has(entry)) continue
      seen.add(entry)
      keys.push(entry)
    }
    return keys
  } catch {
    return []
  }
}

export function saveCollapsedGroupKeys(keys: Iterable<string>): void {
  if (typeof window === 'undefined') return
  try {
    window.localStorage.setItem(COLLAPSED_GROUPS_STORAGE_KEY, JSON.stringify([...keys]))
  } catch {
    /* ignore */
  }
}
