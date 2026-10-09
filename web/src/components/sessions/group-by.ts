/**
 * How the sessions list is grouped. Shared by the page (which groups and
 * sorts) and the group header (which renders the label), so it lives on its
 * own rather than in either.
 */

export type GroupBy = 'none' | 'cwd' | 'command' | 'tag'
