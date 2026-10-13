import { describe, expect, it, vi } from 'vitest'

import type { ListParams } from '@/api/client'
import type { SessionSummary } from '@/api/types'

import {
  runSessionLoad,
  type SessionLoadArgs,
  type SessionLoadRefs,
  type SessionLoadSinks,
} from './sessions-page-loaders'

/**
 * These tests pin the staleness contract, which is the part of the sessions
 * list that can silently show the wrong data: a load for a node or filter the
 * user has already left applies its response anyway.
 */

const ITEMS: SessionSummary[] = [
  {
    id: 's1',
    command: 'bash',
    args: [],
    status: 'running',
    created_at: 0,
    last_total_bytes: 10,
  } as unknown as SessionSummary,
]

const QUERY = {
  search: '',
  statusFilter: 'all' as const,
  page: 0,
  pageSize: 15,
  sortField: 'created_at' as ListParams['sort'],
  sortOrder: 'desc' as ListParams['order'],
}

function harness(overrides: {
  node?: string | null
  hasLoaded?: boolean
  fetch?: SessionLoadArgs['fetch']
}) {
  const refs: SessionLoadRefs = {
    selectedNode: { current: overrides.node ?? null },
    requestVersion: { current: 0 },
    hasLoaded: { current: overrides.hasLoaded ?? false },
    mounted: { current: true },
  }
  const calls = {
    applySessionItems: vi.fn(),
    setRemoteTotal: vi.fn(),
    setLoading: vi.fn(),
    setRefreshing: vi.fn(),
    setLoadError: vi.fn(),
  }
  const sinks: SessionLoadSinks = {
    ...calls,
    getErrorMessage: (_error, fallback) => fallback,
  }
  const args: SessionLoadArgs = {
    node: overrides.node ?? null,
    opts: undefined,
    query: QUERY,
    refs,
    sinks,
    fetch:
      overrides.fetch ??
      (vi.fn(async () => ({ items: ITEMS, total: 1 })) as unknown as SessionLoadArgs['fetch']),
  }
  return { refs, sinks, calls, args }
}

describe('runSessionLoad — the guard', () => {
  it('does nothing when the requested node is not the committed one', async () => {
    // A local load asked for while a remote node is committed.
    const { calls, args } = harness({ node: null })
    args.refs.selectedNode.current = 'lab'

    await runSessionLoad(args)

    expect(args.fetch).not.toHaveBeenCalled()
    expect(calls.setLoading).not.toHaveBeenCalled()
    expect(calls.applySessionItems).not.toHaveBeenCalled()
  })

  it('does nothing for a remote load when no node is committed', async () => {
    const { calls, args } = harness({ node: 'lab' })
    args.refs.selectedNode.current = null

    await runSessionLoad(args)

    expect(args.fetch).not.toHaveBeenCalled()
    expect(calls.setLoading).not.toHaveBeenCalled()
  })
})

describe('runSessionLoad — the response', () => {
  it('applies items and total on success', async () => {
    const { calls, refs, args } = harness({ node: null })

    await runSessionLoad(args)

    expect(calls.applySessionItems).toHaveBeenCalledWith(ITEMS)
    expect(calls.setRemoteTotal).toHaveBeenCalledWith(1)
    expect(refs.hasLoaded.current).toBe(true)
    expect(refs.requestVersion.current).toBe(1)
  })

  it('asks for the node it was given, and for nothing when local', async () => {
    const local = harness({ node: null })
    await runSessionLoad(local.args)
    expect(local.args.fetch).toHaveBeenCalledTimes(1)
    expect('node' in (local.args.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0]).toBe(false)

    const remote = harness({ node: 'lab' })
    await runSessionLoad(remote.args)
    expect('node' in (remote.args.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0]).toBe(true)
    expect((remote.args.fetch as ReturnType<typeof vi.fn>).mock.calls[0][0].node).toBe('lab')
  })

  it('discards a response that arrived after a newer load started', async () => {
    let release: (value: { items: SessionSummary[]; total: number }) => void = () => {}
    const slow = new Promise<{ items: SessionSummary[]; total: number }>((resolve) => {
      release = resolve
    })
    const { calls, args } = harness({
      node: null,
      fetch: (() => slow) as unknown as SessionLoadArgs['fetch'],
    })

    const first = runSessionLoad(args)
    // A second load starts while the first is still in flight.
    args.refs.requestVersion.current += 1
    // ...and then the first one finally answers.
    release({ items: ITEMS, total: 1 })
    await first

    expect(calls.applySessionItems).not.toHaveBeenCalled()
    expect(calls.setRefreshing).not.toHaveBeenCalled()
  })
})

describe('runSessionLoad — loading flags', () => {
  it('shows the skeleton for a first, foreground load', async () => {
    const { calls, args } = harness({ node: null, hasLoaded: false })

    await runSessionLoad(args)

    expect(calls.setLoading).toHaveBeenCalledWith(true)
    expect(calls.setRefreshing).not.toHaveBeenCalledWith(true)
    expect(calls.setLoading).toHaveBeenLastCalledWith(false)
  })

  it('shows the spinner once something has loaded and the load says so', async () => {
    const { calls, args } = harness({ node: null, hasLoaded: true })
    args.opts = {}

    await runSessionLoad(args)

    expect(calls.setLoading).not.toHaveBeenCalledWith(true)
    expect(calls.setRefreshing).toHaveBeenCalledWith(true)
  })

  it('takes the skeleton for a bare load even after something has loaded', async () => {
    // `loadLocal()` with no options is an explicit foreground load, so it
    // re-shows the skeleton. This is the `!opts` arm of the original
    // condition, and it is easy to drop when the two loaders are unified.
    const { calls, args } = harness({ node: null, hasLoaded: true })

    await runSessionLoad(args)

    expect(calls.setLoading).toHaveBeenCalledWith(true)
    expect(calls.setRefreshing).not.toHaveBeenCalledWith(true)
  })

  it('shows neither loading flag for a background load, only the spinner', async () => {
    const { calls, args } = harness({ node: null, hasLoaded: true })
    args.opts = { background: true }

    await runSessionLoad(args)

    // A background refresh never raises the skeleton. It may (and now does)
    // clear the loading flag, because the current load owns the flags — that is
    // what keeps a superseded foreground load from leaving it stuck.
    expect(calls.setLoading).not.toHaveBeenCalledWith(true)
    expect(calls.setRefreshing).toHaveBeenCalledWith(true)
  })

  it("leaves a foreground load's skeleton alone until a newer load settles", async () => {
    // The real race: a foreground load is in flight with the skeleton up
    // (the search just changed), an SSE-triggered background refresh
    // supersedes it, and the foreground response lands after the bump. The
    // loser must not apply or clear anything — but it must not leave the
    // skeleton up either, so the winner owning the flags is what ends the
    // state.
    const { refs } = harness({ node: null, hasLoaded: true })
    const foregroundCalls = {
      applySessionItems: vi.fn(),
      setRemoteTotal: vi.fn(),
      setLoading: vi.fn(),
      setRefreshing: vi.fn(),
      setLoadError: vi.fn(),
    }
    let releaseForeground: (() => void) | undefined
    const foreground: SessionLoadArgs = {
      node: null,
      opts: undefined,
      query: QUERY,
      refs,
      sinks: { ...foregroundCalls, getErrorMessage: (_e, fallback) => fallback },
      fetch: async () => {
        await new Promise<void>((resolve) => {
          releaseForeground = resolve
        })
        return { items: ITEMS, total: 1 }
      },
    }

    const inFlight = runSessionLoad(foreground)
    expect(foregroundCalls.setLoading).toHaveBeenCalledWith(true)

    // The background winner shares the page's version counter and sinks.
    const background = harness({ node: null, hasLoaded: true })
    background.args.refs = refs
    background.args.opts = { background: true }
    await runSessionLoad(background.args)

    releaseForeground?.()
    await inFlight

    // The stale foreground applied and cleared nothing…
    expect(foregroundCalls.applySessionItems).not.toHaveBeenCalled()
    expect(foregroundCalls.setLoading).not.toHaveBeenCalledWith(false)
    // …and the background winner both applied the rows and ended the loading
    // state, so the list renders instead of hanging empty on a stuck flag.
    expect(background.calls.applySessionItems).toHaveBeenCalledWith(ITEMS)
    expect(background.calls.setLoading).toHaveBeenLastCalledWith(false)
    expect(background.calls.setRefreshing).toHaveBeenLastCalledWith(false)
  })
})

describe('runSessionLoad — errors', () => {
  it('reports a foreground failure with the local/remote wording', async () => {
    const local = harness({ node: null })
    local.args.fetch = (async () => {
      throw new Error('boom')
    }) as unknown as SessionLoadArgs['fetch']
    await runSessionLoad(local.args)
    expect(local.calls.setLoadError).toHaveBeenCalledWith({
      title: 'Unable to load sessions',
      message: 'Failed to load local sessions.',
    })

    const remote = harness({ node: 'lab' })
    remote.args.fetch = (async () => {
      throw new Error('boom')
    }) as unknown as SessionLoadArgs['fetch']
    await runSessionLoad(remote.args)
    expect(remote.calls.setLoadError).toHaveBeenCalledWith({
      title: 'Unable to load sessions',
      message: 'Failed to load remote sessions.',
    })
  })

  it('stays quiet for a background failure', async () => {
    const { calls, args } = harness({ node: null })
    args.opts = { background: true }
    args.fetch = (async () => {
      throw new Error('boom')
    }) as unknown as SessionLoadArgs['fetch']

    await runSessionLoad(args)

    expect(calls.setLoadError).not.toHaveBeenCalled()
  })

  it('reports a background failure when asked to', async () => {
    const { calls, args } = harness({ node: null })
    args.opts = { background: true, reportError: true }
    args.fetch = (async () => {
      throw new Error('boom')
    }) as unknown as SessionLoadArgs['fetch']

    await runSessionLoad(args)

    expect(calls.setLoadError).toHaveBeenCalledOnce()
  })

  it('does not report an error for a response that is already stale', async () => {
    let reject: (error: unknown) => void = () => {}
    const pending = new Promise((_, rejectFn) => {
      reject = rejectFn
    })
    const { calls, args } = harness({
      node: null,
      fetch: (() => pending) as unknown as SessionLoadArgs['fetch'],
    })

    const load = runSessionLoad(args)
    args.refs.requestVersion.current += 1
    reject(new Error('boom'))
    await load

    expect(calls.setLoadError).not.toHaveBeenCalled()
  })
})
