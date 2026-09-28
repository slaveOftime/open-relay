import { afterEach, describe, expect, it, vi } from 'vitest'
import { forceRemoveSession, removeSession } from './client'

afterEach(() => vi.unstubAllGlobals())

describe('session deletion requests', () => {
  it('uses non-force removal for a stopped session and force for a running session', async () => {
    vi.stubGlobal('localStorage', { getItem: () => null })
    const fetchMock = vi.fn(
      async () =>
        new Response('{"removed":true}', {
          headers: { 'Content-Type': 'application/json' },
        })
    )
    vi.stubGlobal('fetch', fetchMock)

    await removeSession('stopped id', 'worker-a')
    expect(fetchMock).toHaveBeenNthCalledWith(
      1,
      '/api/sessions/stopped%20id?node=worker-a',
      expect.objectContaining({ method: 'DELETE' })
    )

    await forceRemoveSession('running-id', 'worker-b')
    expect(fetchMock).toHaveBeenNthCalledWith(
      2,
      '/api/sessions/running-id?force=true&node=worker-b',
      expect.objectContaining({ method: 'DELETE' })
    )
  })
})
