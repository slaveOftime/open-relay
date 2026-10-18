import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import {
  COLLAPSED_GROUPS_STORAGE_KEY,
  collapsedGroupKey,
  loadCollapsedGroupKeys,
  saveCollapsedGroupKeys,
} from './group-by'

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

describe('collapsedGroupKey', () => {
  it('namespaces a label by the grouping that produced it', () => {
    expect(collapsedGroupKey('command', 'bash')).toBe('command\u0000bash')
    expect(collapsedGroupKey('tag', 'bash')).not.toBe(collapsedGroupKey('command', 'bash'))
  })
})

describe('collapsed group storage', () => {
  it('round-trips the collapsed keys under the storage key', () => {
    const win = fakeWindow()
    vi.stubGlobal('window', win)

    saveCollapsedGroupKeys(new Set(['command\u0000bash', 'tag\u0000ops']))

    expect(win.__data.get(COLLAPSED_GROUPS_STORAGE_KEY)).toBe(
      JSON.stringify(['command\u0000bash', 'tag\u0000ops'])
    )
    expect(loadCollapsedGroupKeys()).toEqual(['command\u0000bash', 'tag\u0000ops'])
  })

  it('returns nothing without a stored entry or without a window at all', () => {
    expect(loadCollapsedGroupKeys()).toEqual([])

    vi.stubGlobal('window', undefined)
    expect(loadCollapsedGroupKeys()).toEqual([])
    expect(() => saveCollapsedGroupKeys(['x'])).not.toThrow()
  })

  it('returns nothing for corrupt JSON and non-array payloads', () => {
    for (const raw of ['{not json', '{"a":1}', '"a"', 'null']) {
      vi.stubGlobal('window', fakeWindow({ [COLLAPSED_GROUPS_STORAGE_KEY]: raw }))
      expect(loadCollapsedGroupKeys()).toEqual([])
    }
  })

  it('drops entries that are not unique non-empty strings', () => {
    vi.stubGlobal(
      'window',
      fakeWindow({
        [COLLAPSED_GROUPS_STORAGE_KEY]: JSON.stringify(['a', 7, '', 'a', null, 'b', 'b']),
      })
    )

    expect(loadCollapsedGroupKeys()).toEqual(['a', 'b'])
  })

  it('degrades to nothing when storage throws', () => {
    vi.stubGlobal('window', {
      localStorage: {
        getItem: () => {
          throw new Error('denied')
        },
        setItem: () => {
          throw new Error('denied')
        },
      },
    })

    expect(loadCollapsedGroupKeys()).toEqual([])
    expect(() => saveCollapsedGroupKeys(['x'])).not.toThrow()
  })
})
