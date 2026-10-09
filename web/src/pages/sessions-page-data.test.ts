import { describe, expect, it } from 'vitest'

import { SortOrder, SessionSortField } from '@/api/types'

import { buildSessionListParams, getSessionListRequestKey } from './sessions-page-data'

describe('buildSessionListParams', () => {
  it('leaves search, status and node unset when they are empty', () => {
    const params = buildSessionListParams({
      search: '',
      statusFilter: 'all',
      page: 0,
      pageSize: 15,
      sortField: SessionSortField.CreatedAt,
      sortOrder: SortOrder.Desc,
    })

    // `|| undefined` rather than deleting the key: `fetchSessions` skips
    // falsy query params, so an undefined value behaves as absent.
    expect(params.search).toBeUndefined()
    expect(params.status).toBeUndefined()
    expect(params.node).toBeUndefined()
    expect(params).toMatchObject({
      limit: 15,
      offset: 0,
      sort: SessionSortField.CreatedAt,
      order: SortOrder.Desc,
    })
  })

  it('adds node only when one is selected', () => {
    const withNode = buildSessionListParams({
      search: '',
      statusFilter: 'all',
      page: 1,
      pageSize: 15,
      sortField: SessionSortField.CreatedAt,
      sortOrder: SortOrder.Desc,
      node: 'lab',
    })

    expect(withNode.node).toBe('lab')
    // offset is page * pageSize, so page 1 of 15 skips the first 15 rows.
    expect(withNode.offset).toBe(15)
  })

  it('keeps whitespace-only search as a real search term', () => {
    const params = buildSessionListParams({
      search: '  ',
      statusFilter: 'all',
      page: 0,
      pageSize: 15,
      sortField: SessionSortField.CreatedAt,
      sortOrder: SortOrder.Desc,
    })

    // The user typed something; do not silently turn it into "everything".
    expect(params.search).toBe('  ')
  })

  it('produces the local and remote shapes the loaders used to build by hand', () => {
    const shared = {
      search: 'zsh',
      statusFilter: 'stopped' as const,
      page: 2,
      pageSize: 25,
      sortField: SessionSortField.Title,
      sortOrder: SortOrder.Asc,
    }

    expect(buildSessionListParams(shared)).toEqual({
      search: 'zsh',
      status: 'stopped',
      limit: 25,
      offset: 50,
      sort: SessionSortField.Title,
      order: SortOrder.Asc,
    })
    expect(buildSessionListParams({ ...shared, node: 'lab' })).toEqual({
      search: 'zsh',
      status: 'stopped',
      limit: 25,
      offset: 50,
      sort: SessionSortField.Title,
      order: SortOrder.Asc,
      node: 'lab',
    })
  })
})

describe('getSessionListRequestKey', () => {
  it('distinguishes the params that change what is fetched', () => {
    const base = buildSessionListParams({
      search: 'a',
      statusFilter: 'all',
      page: 0,
      pageSize: 15,
      sortField: SessionSortField.Id,
      sortOrder: SortOrder.Desc,
    })

    const same = getSessionListRequestKey(base)
    expect(getSessionListRequestKey({ ...base })).toBe(same)

    // Every one of these is a different HTTP request, so a different key.
    // (The key is built from the resolved ListParams, so page changes show
    // up as an offset change.)
    expect(getSessionListRequestKey({ ...base, offset: 15 })).not.toBe(same)
    expect(getSessionListRequestKey({ ...base, limit: 25 })).not.toBe(same)
    expect(getSessionListRequestKey({ ...base, sort: SessionSortField.Title })).not.toBe(same)
    expect(getSessionListRequestKey({ ...base, order: SortOrder.Asc })).not.toBe(same)
    expect(getSessionListRequestKey({ ...base, node: 'lab' })).not.toBe(same)
    expect(getSessionListRequestKey({ ...base, status: 'stopped' })).not.toBe(same)
    expect(getSessionListRequestKey({ ...base, search: 'b' })).not.toBe(same)
  })

  it('treats an absent value and an explicitly empty one as the same request', () => {
    const withEmpty = getSessionListRequestKey({ search: '', status: undefined })
    const withNothing = getSessionListRequestKey({})

    expect(withEmpty).toBe(withNothing)
  })
})
