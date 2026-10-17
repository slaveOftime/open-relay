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
