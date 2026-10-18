import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import {
  SESSIONS_SCROLL_STORAGE_KEY,
  loadSessionsScrollTop,
  saveSessionsScrollTop,
} from './sessions-page-scroll'

function fakeWindow(stored: Record<string, string> = {}) {
  const data = new Map(Object.entries(stored))
  return {
    sessionStorage: {
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

describe('sessions scroll storage', () => {
  it('round-trips an offset through sessionStorage', () => {
    const win = fakeWindow()
    vi.stubGlobal('window', win)

    saveSessionsScrollTop(640.8)

    expect(win.__data.get(SESSIONS_SCROLL_STORAGE_KEY)).toBe('640')
    expect(loadSessionsScrollTop()).toBe(640)
  })

  it('treats a missing, non-numeric, or non-positive entry as the top', () => {
    for (const raw of [undefined, 'abc', '0', '-5', 'NaN', 'Infinity']) {
      vi.stubGlobal(
        'window',
        fakeWindow(raw === undefined ? {} : { [SESSIONS_SCROLL_STORAGE_KEY]: raw })
      )
      expect(loadSessionsScrollTop()).toBe(0)
    }
  })

  it('removes the entry when the list is back at the top', () => {
    const win = fakeWindow({ [SESSIONS_SCROLL_STORAGE_KEY]: '320' })
    vi.stubGlobal('window', win)

    saveSessionsScrollTop(0)
    expect(win.__data.has(SESSIONS_SCROLL_STORAGE_KEY)).toBe(false)

    saveSessionsScrollTop(Number.NaN)
    expect(win.__data.has(SESSIONS_SCROLL_STORAGE_KEY)).toBe(false)
  })

  it('returns the top and stays quiet without a window at all', () => {
    vi.stubGlobal('window', undefined)

    expect(loadSessionsScrollTop()).toBe(0)
    expect(() => saveSessionsScrollTop(100)).not.toThrow()
  })

  it('degrades to the top when storage throws', () => {
    vi.stubGlobal('window', {
      sessionStorage: {
        getItem: () => {
          throw new Error('denied')
        },
        setItem: () => {
          throw new Error('denied')
        },
        removeItem: () => {
          throw new Error('denied')
        },
      },
    })

    expect(loadSessionsScrollTop()).toBe(0)
    expect(() => saveSessionsScrollTop(100)).not.toThrow()
  })
})
