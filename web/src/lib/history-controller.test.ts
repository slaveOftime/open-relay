import { describe, expect, it } from 'vitest'

import { HistoryAnchorError, HistoryController, type HistoryPage } from './history-controller'

function page(
  offset: number,
  chunkTexts: string[],
  total: number,
  resizes: HistoryPage['resizes'] = []
): HistoryPage {
  return {
    offset,
    chunks: chunkTexts.map((t) => new TextEncoder().encode(t)),
    total,
    resizes,
  }
}

function texts(chunks: readonly Uint8Array[]): string[] {
  return chunks.map((c) => new TextDecoder().decode(c))
}

describe('HistoryController', () => {
  it('loads the initial page from offset 0', async () => {
    const controller = new HistoryController(async () => page(0, ['a', 'b'], 4), 2)
    expect(await controller.loadInitial()).toBe(2)
    expect(texts(controller.getChunks())).toEqual(['a', 'b'])
    expect(controller.totalChunks).toBe(4)
    expect(controller.loaded).toBe(2)
    expect(controller.hasMore).toBe(true)
  })

  it('pages forward anchored at the loaded offset without duplication', async () => {
    const pages = new Map<number, HistoryPage>([
      [0, page(0, ['a', 'b'], 6)],
      [2, page(2, ['c', 'd'], 6)],
      [4, page(4, ['e', 'f'], 6)],
    ])
    const requested: number[] = []
    const controller = new HistoryController(async (offset) => {
      requested.push(offset)
      const found = pages.get(offset)
      if (!found) throw new Error(`unexpected offset ${offset}`)
      return found
    }, 2)

    expect(await controller.loadInitial()).toBe(2)
    expect(await controller.loadMore()).toBe(2)
    expect(await controller.loadMore()).toBe(2)
    expect(texts(controller.getChunks())).toEqual(['a', 'b', 'c', 'd', 'e', 'f'])
    expect(requested).toEqual([0, 2, 4])
    expect(controller.hasMore).toBe(false)
    // Nothing more to load: no further fetch, no growth.
    expect(await controller.loadMore()).toBe(0)
    expect(requested).toEqual([0, 2, 4])
  })

  it('collapses concurrent loadMore calls into one fetch', async () => {
    let fetches = 0
    let release!: (page: HistoryPage) => void
    const gate = new Promise<HistoryPage>((resolve) => {
      release = resolve
    })
    const controller = new HistoryController(async () => {
      fetches += 1
      return gate
    }, 2)

    const first = controller.loadMore()
    const second = controller.loadMore()
    expect(controller.isFetching).toBe(true)
    release(page(0, ['a'], 1))
    expect(await first).toBe(1)
    expect(await second).toBe(0)
    expect(fetches).toBe(1)
    expect(controller.isFetching).toBe(false)
  })

  it('throws loudly on an anchor mismatch instead of duplicating history', async () => {
    const controller = new HistoryController(async (offset) => {
      // Server answers with a different offset than requested.
      return page(offset + 1, ['x'], 10)
    }, 2)
    await expect(controller.loadInitial()).rejects.toBeInstanceOf(HistoryAnchorError)
    expect(controller.chunkCount).toBe(0)
  })

  it('discards a stale in-flight page after reset', async () => {
    let release!: (page: HistoryPage) => void
    const gate = new Promise<HistoryPage>((resolve) => {
      release = resolve
    })
    let calls = 0
    const controller = new HistoryController(async () => {
      calls += 1
      return calls === 1 ? gate : page(0, ['fresh'], 1)
    }, 2)

    const stale = controller.loadMore()
    controller.reset()
    expect(controller.isFetching).toBe(false)
    release(page(0, ['stale'], 99))
    expect(await stale).toBe(0)
    expect(controller.chunkCount).toBe(0)
    expect(controller.totalChunks).toBe(0)

    expect(await controller.loadInitial()).toBe(1)
    expect(texts(controller.getChunks())).toEqual(['fresh'])
  })

  it('clamps replay indices into the loaded range', async () => {
    const controller = new HistoryController(async () => page(0, ['a', 'b'], 2), 2)
    expect(controller.clampIndex(-5)).toBe(0)
    expect(controller.clampIndex(10)).toBe(0)
    await controller.loadInitial()
    expect(controller.clampIndex(-5)).toBe(0)
    expect(controller.clampIndex(1)).toBe(1)
    expect(controller.clampIndex(10)).toBe(2)
  })

  it('treats an empty page as end-of-history and stops advancing', async () => {
    let calls = 0
    const controller = new HistoryController(async (offset) => {
      calls += 1
      return page(offset, [], 0)
    }, 2)
    expect(await controller.loadMore()).toBe(0)
    expect(controller.chunkCount).toBe(0)
    // Unknown total keeps hasMore true, but a second empty page is a no-op.
    expect(await controller.loadMore()).toBe(0)
    expect(calls).toBe(2)
  })
})
