import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { SortOrder, SessionSortField } from '@/api/types'
import {
  getOrderedSessionTableColumns,
  type SessionTableColumnSizes,
} from '@/lib/sessions-table-columns'

vi.mock('./sessions-page-events', () => ({
  matchesStatusFilter: (status: string, value: string) => status === value,
  normalizeStoredNode: (node: unknown) =>
    typeof node === 'string' && node.trim() !== '' ? node.trim() : null,
}))

const {
  DEFAULT_PAGE_SIZE,
  PAGE_SIZE_OPTIONS,
  SORT_OPTIONS,
  filterSessionsByStatus,
  getErrorMessage,
  loadSessionPrefs,
  loadSessionTableColumnSettings,
  normalizeStatusFilter,
  saveSessionPrefs,
  saveSessionTableColumnSettings,
  sessionPageTitle,
} = await import('./sessions-page-prefs')

function fakeWindow(stored: Record<string, string> = {}) {
  const data = new Map(Object.entries(stored))
  return {
    localStorage: {
      getItem: (key: string) => (data.has(key) ? data.get(key)! : null),
      setItem: (key: string, value: string) => void data.set(key, value),
      removeItem: (key: string) => void data.delete(key),
    },
    __data: data,
  }
}

beforeEach(() => {
  vi.stubGlobal('window', fakeWindow())
})

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('loadSessionPrefs', () => {
  it('returns the defaults with no window at all', () => {
    vi.unstubAllGlobals()
    vi.stubGlobal('window', undefined)

    const prefs = loadSessionPrefs()

    expect(prefs).toEqual({
      search: '',
      statusFilter: 'all',
      groupBy: 'none',
      node: null,
      sortField: SessionSortField.CreatedAt,
      sortOrder: SortOrder.Desc,
      pageSize: DEFAULT_PAGE_SIZE,
    })
  })

  it('keeps every valid stored value', () => {
    vi.stubGlobal(
      'window',
      fakeWindow({
        'open-relay.webv2.sessions.preferences.v1': JSON.stringify({
          search: 'zsh',
          statusFilter: 'stopped',
          groupBy: 'command',
          node: 'lab',
          sortField: SessionSortField.Title,
          sortOrder: SortOrder.Asc,
          pageSize: 50,
        }),
      })
    )

    expect(loadSessionPrefs()).toEqual({
      search: 'zsh',
      statusFilter: 'stopped',
      groupBy: 'command',
      node: 'lab',
      sortField: SessionSortField.Title,
      sortOrder: SortOrder.Asc,
      pageSize: 50,
    })
  })

  it('falls back to the legacy key when the current key is absent', () => {
    vi.stubGlobal(
      'window',
      fakeWindow({ 'open-relay.sessions.preferences.v1': JSON.stringify({ search: 'old' }) })
    )

    expect(loadSessionPrefs().search).toBe('old')
  })

  it('returns defaults for corrupt JSON', () => {
    vi.stubGlobal('window', fakeWindow({ 'open-relay.webv2.sessions.preferences.v1': '{not json' }))

    expect(loadSessionPrefs().search).toBe('')
  })

  it('rejects out-of-range and fractional page sizes', () => {
    for (const bad of [0, -5, 501, 10.5]) {
      vi.stubGlobal(
        'window',
        fakeWindow({
          'open-relay.webv2.sessions.preferences.v1': JSON.stringify({ pageSize: bad }),
        })
      )
      expect(loadSessionPrefs().pageSize).toBe(DEFAULT_PAGE_SIZE)
    }
  })

  it('rejects unknown enum values and falls back per field', () => {
    vi.stubGlobal(
      'window',
      fakeWindow({
        'open-relay.webv2.sessions.preferences.v1': JSON.stringify({
          statusFilter: 'bogus',
          groupBy: 'bogus',
          sortField: 'bogus',
          sortOrder: 'bogus',
          search: 42,
        }),
      })
    )

    const prefs = loadSessionPrefs()

    expect(prefs.statusFilter).toBe('all')
    expect(prefs.groupBy).toBe('none')
    expect(prefs.sortField).toBe(SessionSortField.CreatedAt)
    expect(prefs.sortOrder).toBe(SortOrder.Desc)
    expect(prefs.search).toBe('')
  })
})

describe('saveSessionPrefs', () => {
  it('writes under the current key', () => {
    const win = fakeWindow()
    vi.stubGlobal('window', win)

    saveSessionPrefs({
      search: 'a',
      statusFilter: 'all',
      groupBy: 'none',
      node: null,
      sortField: SessionSortField.Id,
      sortOrder: SortOrder.Desc,
      pageSize: DEFAULT_PAGE_SIZE,
    })

    expect(win.__data.has('open-relay.webv2.sessions.preferences.v1')).toBe(true)
    expect(win.__data.has('open-relay.sessions.preferences.v1')).toBe(false)
  })
})

describe('filterSessionsByStatus and normalizeStatusFilter', () => {
  const sessions = [
    { status: 'running' },
    { status: 'stopped' },
    { status: 'running' },
  ] as Parameters<typeof filterSessionsByStatus>[0]

  it('returns the input unchanged for the "all" filter', () => {
    expect(filterSessionsByStatus(sessions, 'all')).toBe(sessions)
  })

  it('keeps only matching rows otherwise', () => {
    expect(filterSessionsByStatus(sessions, 'running')).toHaveLength(2)
    expect(filterSessionsByStatus(sessions, 'stopped')).toHaveLength(1)
  })

  it('normalizes anything unknown to "all"', () => {
    expect(normalizeStatusFilter('running')).toBe('running')
    expect(normalizeStatusFilter('nope')).toBe('all')
    expect(normalizeStatusFilter(undefined)).toBe('all')
  })
})

describe('sessionPageTitle', () => {
  it('is empty for local or absent nodes and the node name otherwise', () => {
    expect(sessionPageTitle(null)).toBe('')
    expect(sessionPageTitle('local')).toBe('')
    expect(sessionPageTitle('lab')).toBe('lab')
  })
})

describe('getErrorMessage', () => {
  it('uses the error message when it has content', () => {
    expect(getErrorMessage(new Error('boom'), 'fallback')).toBe('boom')
  })

  it('uses the fallback for empty messages and non-Errors', () => {
    expect(getErrorMessage(new Error('   '), 'fallback')).toBe('fallback')
    expect(getErrorMessage('a string', 'fallback')).toBe('fallback')
    expect(getErrorMessage(null, 'fallback')).toBe('fallback')
  })
})

describe('table column settings', () => {
  it('round-trips through localStorage', () => {
    const defaults = getOrderedSessionTableColumns([])
    const sizes = Object.fromEntries(
      defaults.map((column) => [column.key, column.defaultWidth])
    ) as SessionTableColumnSizes
    const order = defaults.map((column) => column.key)
    saveSessionTableColumnSettings({ sizes, order })

    expect(loadSessionTableColumnSettings().sizes.id).toBe(sizes.id)
    expect(loadSessionTableColumnSettings().order).toEqual(order)
  })

  it('falls back to the coerced defaults for corrupt values', () => {
    vi.stubGlobal('window', fakeWindow({ 'open-relay.webv2.sessions.table-columns.v1': '<nope>' }))

    expect(loadSessionTableColumnSettings().order).toEqual(expect.any(Array))
  })
})

describe('option constants', () => {
  it('exposes the page sizes in ascending order', () => {
    expect(PAGE_SIZE_OPTIONS).toEqual([10, 15, 25, 50, 100])
  })

  it('offers one sort option per sortable field', () => {
    expect(SORT_OPTIONS.map((option) => option.value)).toEqual([
      SessionSortField.CreatedAt,
      SessionSortField.Status,
      SessionSortField.Title,
      SessionSortField.Id,
      SessionSortField.Command,
      SessionSortField.Cwd,
      SessionSortField.Pid,
    ])
  })
})
