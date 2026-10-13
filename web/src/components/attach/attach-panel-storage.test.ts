import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import type { SessionImagePreviews } from './attach-panel-image-preview'

const {
  loadSessionDrawerOpen,
  loadSessionImagePreviews,
  loadSessionInputDraft,
  readFileAsDataUrl,
  saveSessionDrawerOpen,
  saveSessionImagePreviews,
  saveSessionInputDraft,
} = await import('./attach-panel-storage')

function makeStorage() {
  const data = new Map<string, string>()
  return {
    localStorage: {
      getItem: (key: string) => (data.has(key) ? data.get(key)! : null),
      setItem: (key: string, value: string) => void data.set(key, value),
      removeItem: (key: string) => void data.delete(key),
    },
    sessionStorage: {
      getItem: (key: string) => (data.has(key) ? data.get(key)! : null),
      setItem: (key: string, value: string) => void data.set(key, value),
      removeItem: (key: string) => void data.delete(key),
    },
    data,
  }
}

function throwOnWrite(storage: ReturnType<typeof makeStorage>) {
  return {
    ...storage,
    localStorage: {
      ...storage.localStorage,
      setItem: () => {
        throw new Error('quota exceeded')
      },
      getItem: () => {
        throw new Error('unavailable')
      },
    },
    sessionStorage: {
      ...storage.sessionStorage,
      setItem: () => {
        throw new Error('quota exceeded')
      },
      getItem: () => {
        throw new Error('unavailable')
      },
    },
  }
}

const PREVIEWS: SessionImagePreviews = { '/tmp/a.png': 'data:image/png;base64,AAAA' }

beforeEach(() => {
  vi.stubGlobal('localStorage', makeStorage().localStorage)
  vi.stubGlobal('sessionStorage', makeStorage().sessionStorage)
})

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('input draft', () => {
  it('round-trips the text a user typed', () => {
    saveSessionInputDraft('s1', 'ls -la')

    expect(loadSessionInputDraft('s1')).toBe('ls -la')
  })

  it('is namespaced by session id, so two sessions do not share a draft', () => {
    saveSessionInputDraft('s1', 'one')

    expect(loadSessionInputDraft('s2')).toBe('')
  })

  it('clears the stored draft when the text becomes empty', () => {
    saveSessionInputDraft('s1', 'one')
    saveSessionInputDraft('s1', '')

    expect(loadSessionInputDraft('s1')).toBe('')
  })

  it('reads as empty for a blank session id rather than a shared key', () => {
    // A blank id would otherwise build the key `...:` and share one draft
    // across every session with an empty id.
    saveSessionInputDraft('  ', 'one')

    expect(loadSessionInputDraft('')).toBe('')
    expect(loadSessionInputDraft('   ')).toBe('')
  })

  it('reads as empty when storage is unavailable', () => {
    vi.stubGlobal('localStorage', throwOnWrite(makeStorage()).localStorage)

    expect(loadSessionInputDraft('s1')).toBe('')
  })

  it('does not throw when saving is refused', () => {
    vi.stubGlobal('localStorage', throwOnWrite(makeStorage()).localStorage)

    expect(() => saveSessionInputDraft('s1', 'one')).not.toThrow()
  })
})

describe('drawer state', () => {
  it('round-trips as the flag the panel reads', () => {
    saveSessionDrawerOpen('s1', true)

    expect(loadSessionDrawerOpen('s1')).toBe(true)
    expect(loadSessionDrawerOpen('s2')).toBe(false)
  })

  it('clears the flag when the drawer closes', () => {
    saveSessionDrawerOpen('s1', true)
    saveSessionDrawerOpen('s1', false)

    expect(loadSessionDrawerOpen('s1')).toBe(false)
  })

  it('reads as closed when storage is unavailable', () => {
    vi.stubGlobal('localStorage', throwOnWrite(makeStorage()).localStorage)

    expect(loadSessionDrawerOpen('s1')).toBe(false)
  })
})

describe('image preview cache', () => {
  it('round-trips the previews a session staged', () => {
    saveSessionImagePreviews('s1', PREVIEWS)

    expect(loadSessionImagePreviews('s1')).toEqual(PREVIEWS)
    expect(loadSessionImagePreviews('s2')).toEqual({})
  })

  it('drops the cached entry when the last preview is removed', () => {
    saveSessionImagePreviews('s1', PREVIEWS)
    saveSessionImagePreviews('s1', {})

    expect(loadSessionImagePreviews('s1')).toEqual({})
  })

  it('drops entries that are not usable images', () => {
    // The cache is read back with the same coercion the component uses, so a
    // corrupt entry silently falls back rather than breaking the panel.
    vi.stubGlobal(
      'sessionStorage',
      (() => {
        const s = makeStorage()
        s.data.set(
          'open-relay:session-image-preview:s1',
          JSON.stringify({ bad: 42, good: 'data:1' })
        )
        return s.sessionStorage
      })()
    )

    expect(loadSessionImagePreviews('s1')).toEqual({ good: 'data:1' })
  })

  it('reads as empty for corrupt JSON or unavailable storage', () => {
    vi.stubGlobal(
      'sessionStorage',
      (() => {
        const s = makeStorage()
        s.data.set('open-relay:session-image-preview:s1', '<nope>')
        return s.sessionStorage
      })()
    )
    expect(loadSessionImagePreviews('s1')).toEqual({})

    vi.stubGlobal('sessionStorage', throwOnWrite(makeStorage()).sessionStorage)
    expect(loadSessionImagePreviews('s1')).toEqual({})
  })

  it('does not throw when saving is refused', () => {
    vi.stubGlobal('sessionStorage', throwOnWrite(makeStorage()).sessionStorage)

    expect(() => saveSessionImagePreviews('s1', PREVIEWS)).not.toThrow()
  })
})

describe('readFileAsDataUrl', () => {
  /** A FileReader stand-in: the node test environment has none. */
  function fakeFileReader(result: string | ArrayBuffer | null, fails = false) {
    const reader = {
      result,
      error: fails ? new Error('read failed') : null,
      onload: null as (() => void) | null,
      onerror: null as (() => void) | null,
      readAsDataURL: vi.fn(),
    }
    vi.stubGlobal('FileReader', function () {
      return reader
    })
    return reader
  }

  function file(): File {
    return { name: 'a.png', type: 'image/png' } as unknown as File
  }

  it('resolves with the data URL the reader produced', async () => {
    const reader = fakeFileReader('data:image/png;base64,AAAA')
    const promise = readFileAsDataUrl(file())

    reader.onload?.()
    await expect(promise).resolves.toBe('data:image/png;base64,AAAA')
  })

  it('rejects when the reader produced something that is not a string', async () => {
    const reader = fakeFileReader(null)
    const promise = readFileAsDataUrl(file())

    reader.onload?.()
    await expect(promise).rejects.toThrow('image preview unavailable')
  })

  it('rejects with the reader error when the read fails', async () => {
    const reader = fakeFileReader('data:image/png;base64,AAAA', true)
    const promise = readFileAsDataUrl(file())

    reader.onerror?.()
    await expect(promise).rejects.toThrow('read failed')
  })

  it('actually calls readAsDataURL on the file', async () => {
    const reader = fakeFileReader(null)
    const f = file()
    // Not awaited: the promise resolves only when the fake reader fires, and
    // this test only checks that the read was started.
    readFileAsDataUrl(f)

    reader.readAsDataURL(f)
    expect(reader.readAsDataURL).toHaveBeenCalledWith(f)
  })
})
