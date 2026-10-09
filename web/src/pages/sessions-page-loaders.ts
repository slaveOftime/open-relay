/**
 * Session-list loading.
 *
 * The two entry points look like duplicates and are not: `loadLocal` and
 * `loadRemote` guard on opposite conditions, report different errors, and end
 * differently. What they *do* share is the part that is easy to get wrong and
 * impossible to see in a diff — the staleness contract. Everything here exists
 * to make that contract explicit and testable:
 *
 * - A load claims a request version. Any load that starts after it wins, and
 *   the loser must not touch state at all, because the user has already moved
 *   on to a different page, a different node, or a different filter.
 * - `opts.background` loads are silent: they must not flip the skeleton on and
 *   must not show an error unless `reportError` is set, because they are
 *   refreshes behind the user's back.
 *
 * `runSessionLoad` is the whole thing as a plain async function so it can be
 * driven from a test with fake sinks; `useSessionLoaders` wires it to React.
 */

import { useCallback, useRef } from 'react'

import type { ListParams } from '@/api/client'
import type { SessionSummary, SessionStatusFilter } from '@/api/types'

import {
  buildSessionListParams,
  fetchSessionsOnce,
  type SessionListQuery,
} from './sessions-page-data'

export interface LoadOptions {
  /** A refresh behind the user's back: no skeleton, no error banner. */
  background?: boolean
  /** Surface errors even for a background load. */
  reportError?: boolean
}

export interface LoadErrorState {
  title: string
  message: string
}

export interface SessionLoadRefs {
  /** Node the ref uses to decide whether a response is still wanted. */
  selectedNode: React.RefObject<string | null>
  /** Bumped by every new load; a load only applies if it is still the newest. */
  requestVersion: React.RefObject<number>
  /** False until the first successful load, which is what decides skeleton vs spinner. */
  hasLoaded: React.RefObject<boolean>
  mounted: React.RefObject<boolean>
}

export interface SessionLoadSinks {
  applySessionItems: (items: SessionSummary[]) => void
  setRemoteTotal: (total: number) => void
  setLoading: (loading: boolean) => void
  setRefreshing: (refreshing: boolean) => void
  setLoadError: (error: LoadErrorState | null) => void
  getErrorMessage: (error: unknown, fallback: string) => string
}

export interface SessionLoadArgs {
  /** The node to load for; `null` is the local instance. */
  node: string | null
  opts: LoadOptions | undefined
  query: SessionListQuery
  refs: SessionLoadRefs
  sinks: SessionLoadSinks
  /** Injected so a test can control both success and timing. */
  fetch: (params: ListParams) => Promise<{ items: SessionSummary[]; total: number }>
}

/**
 * The local and remote loaders are one function: they differ only in which node
 * they ask for and how they describe themselves in an error message.
 */
export async function runSessionLoad(args: SessionLoadArgs): Promise<void> {
  const { node, opts, query, refs, sinks } = args

  // A load for a node that is no longer selected is already stale before it
  // starts — the user switched while this call was being built.
  const nodeStillSelected =
    node === null ? refs.selectedNode.current === null : refs.selectedNode.current === node
  if (!nodeStillSelected) return

  const requestVersion = refs.requestVersion.current + 1
  refs.requestVersion.current = requestVersion

  const isCurrent = () =>
    refs.mounted.current &&
    refs.requestVersion.current === requestVersion &&
    refs.selectedNode.current === node

  const shouldShowSkeleton = !opts?.background && !refs.hasLoaded.current
  // An explicit foreground load (including no options at all) takes the
  // skeleton; only a load that *says* it is background gets the spinner.
  const foreground = !opts || opts.background === false
  if (shouldShowSkeleton || foreground) sinks.setLoading(true)
  else sinks.setRefreshing(true)

  const where = node === null ? 'local' : 'remote'
  try {
    const params = buildSessionListParams({ ...query, node })
    const res = await args.fetch(params)
    // The fetch above is the yield point: a second load, a node change or an
    // unmount can all have happened during it.
    if (!isCurrent()) return

    refs.hasLoaded.current = true
    sinks.applySessionItems(res.items)
    sinks.setRemoteTotal(res.total)
  } catch (error) {
    if (isCurrent() && (!opts?.background || opts?.reportError)) {
      sinks.setLoadError({
        title: 'Unable to load sessions',
        message: sinks.getErrorMessage(error, `Failed to load ${where} sessions.`),
      })
    }
  } finally {
    if (isCurrent()) {
      // Only clear the loading flag if this load is the one that set it. A
      // background refresh that raced a skeleton must not clear it, which is
      // why the two flags are tracked separately.
      if (shouldShowSkeleton || foreground) sinks.setLoading(false)
      sinks.setRefreshing(false)
    }
  }
}

export interface SessionLoadersInput {
  search: string
  statusFilter: SessionStatusFilter
  page: number
  pageSize: number
  sortField: ListParams['sort']
  sortOrder: ListParams['order']
  selectedNode: string | null
  applySessionItems: (items: SessionSummary[]) => void
  setRemoteTotal: (total: number) => void
  setLoading: (loading: boolean) => void
  setRefreshing: (refreshing: boolean) => void
  setLoadError: (error: LoadErrorState | null) => void
  getErrorMessage: (error: unknown, fallback: string) => string
  mounted: React.RefObject<boolean>
}

export interface SessionLoaders {
  selectedNodeRef: React.RefObject<string | null>
  requestVersionRef: React.RefObject<number>
  hasLoadedRef: React.RefObject<boolean>
  loadLocal: (opts?: LoadOptions) => Promise<void>
  loadRemote: (opts?: LoadOptions) => Promise<void>
}

/**
 * Owns the three refs the staleness contract needs, and returns them so the
 * rest of the page can read and bump them exactly as before.
 *
 * `selectedNodeRef` is deliberately **not** synced to the prop here. It is the
 * *committed* node: `handleNodeChange` writes it synchronously, alongside the
 * version bump that invalidates in-flight loads, so a load asked for the old
 * node fails its own guard before the state update lands.
 *
 * Stability is the whole reason the inputs are flat rather than objects. The
 * page feeds this hook on every render, and a `useCallback` whose dependency is
 * a fresh object literal is a new function every render — which silently makes
 * every effect that depends on `loadLocal` / `loadRemote` (and on
 * `reloadSessions`, which depends on them) re-run forever.
 */
export function useSessionLoaders(input: SessionLoadersInput): SessionLoaders {
  const {
    search,
    statusFilter,
    page,
    pageSize,
    sortField,
    sortOrder,
    selectedNode,
    applySessionItems,
    setRemoteTotal,
    setLoading,
    setRefreshing,
    setLoadError,
    getErrorMessage,
    mounted,
  } = input

  const selectedNodeRef = useRef<string | null>(selectedNode)
  const requestVersionRef = useRef(0)
  const hasLoadedRef = useRef(false)
  // Built once: `useRef` keeps the first value, so the object identity is stable
  // for the life of the component even though the refs it holds are not.
  const refsRef = useRef<SessionLoadRefs>({
    selectedNode: selectedNodeRef,
    requestVersion: requestVersionRef,
    hasLoaded: hasLoadedRef,
    mounted,
  })

  const loadLocal = useCallback(
    (opts?: LoadOptions) =>
      runSessionLoad({
        node: null,
        opts,
        query: { search, statusFilter, page, pageSize, sortField, sortOrder },
        refs: refsRef.current,
        sinks: {
          applySessionItems,
          setRemoteTotal,
          setLoading,
          setRefreshing,
          setLoadError,
          getErrorMessage,
        },
        fetch: fetchSessionsOnce,
      }),
    [
      applySessionItems,
      getErrorMessage,
      page,
      pageSize,
      search,
      setLoadError,
      setLoading,
      setRefreshing,
      setRemoteTotal,
      sortField,
      sortOrder,
      statusFilter,
    ]
  )

  const loadRemote = useCallback(
    (opts?: LoadOptions) =>
      runSessionLoad({
        node: selectedNode,
        opts,
        query: { search, statusFilter, page, pageSize, sortField, sortOrder },
        refs: refsRef.current,
        sinks: {
          applySessionItems,
          setRemoteTotal,
          setLoading,
          setRefreshing,
          setLoadError,
          getErrorMessage,
        },
        fetch: fetchSessionsOnce,
      }),
    [
      applySessionItems,
      getErrorMessage,
      page,
      pageSize,
      search,
      selectedNode,
      setLoadError,
      setLoading,
      setRefreshing,
      setRemoteTotal,
      sortField,
      sortOrder,
      statusFilter,
    ]
  )

  return {
    selectedNodeRef,
    requestVersionRef,
    hasLoadedRef,
    loadLocal,
    loadRemote,
  }
}
