import { useState, useEffect, useCallback, useMemo, useRef, Fragment } from 'react'
import { useSearchParams } from 'react-router-dom'
import type { ListParams } from '@/api/client'
import {
  SessionSortField,
  SortOrder,
  isSessionStatusFilter,
  type SessionSummary,
  type SessionStatus,
  type SessionStatusFilter,
  type NodeSummary,
} from '@/api/types'
import {
  fetchSessions,
  stopSession,
  killSession,
  setSessionNotifications,
  fetchNodes,
} from '@/api/client'
import NewSessionDialog from '@/components/dialogs/NewSessionDialog'
import { buildNewSessionInitialValues } from '@/components/dialogs/new-session-dialog-values'
import SessionMetadataDialog from '@/components/dialogs/SessionMetadataDialog'
import SessionDeleteConfirmDialog from '@/components/dialogs/SessionDeleteConfirmDialog'
import { getOrderedSessionTableColumns, getSessionTableWidth } from '@/lib/sessions-table-columns'
import {
  handleSessionPageEvent,
  normalizeStoredNode,
  type SessionPageEventContext,
  type SessionPageEventHandlers,
} from './sessions-page-events'
import {
  applyPendingTermination,
  applyPendingTerminations,
  revertSessionStatus,
  withPendingTermination,
  withSessionStatus,
  type SessionTermination,
} from '@/utils/session-termination'
import { NodeSelector } from '@/components/NodeSelector'
import { agentName, normalizeCwdPath } from '@/utils/format'
import {
  loadPinnedSessionKeys,
  orderSessionPage,
  savePinnedSessionKeys,
  sessionIsPinnable,
  sessionPinKey,
} from '@/utils/session-ordering'
import Logo from '@/components/Logo'
import SseStatusDot from '@/components/SseStatusDot'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { TooltipProvider } from '@/components/ui/tooltip'
import {
  BellIcon,
  CaretDownIcon,
  ChevronLeftIcon,
  ChevronRightIcon,
  Cross2Icon,
  GridIcon,
  MixerHorizontalIcon,
  PlayIcon,
  PlusIcon,
  ReloadIcon,
} from '@radix-ui/react-icons'
import {
  disablePushNotifications,
  showSessionNotification,
  syncPushSubscription,
  type PushSetupState,
} from '@/lib/push'
import { ingestSessionSummaries, subscribeSessionEvents } from '@/lib/session-events'
import { useSseConnectionState } from '@/hooks/use-session-events'
import { useSessionTableColumns } from '@/hooks/use-session-table-columns'
import { useSessionListGestures } from '@/hooks/use-session-list-gestures'
import { GroupHeaderLabel } from '@/components/sessions/GroupHeaderLabel'
import type { GroupBy } from '@/components/sessions/group-by'
import { SessionCard } from '@/components/sessions/SessionCard'
import { SessionRow } from '@/components/sessions/SessionRow'
import { SkeletonCard, SkeletonRow } from '@/components/sessions/SessionSkeletons'
import { SessionsEmptyState } from '@/components/sessions/SessionsEmptyState'
import {
  PAGE_SIZE_OPTIONS,
  SORT_OPTIONS,
  filterSessionsByStatus,
  getErrorMessage,
  loadSessionPrefs,
  loadSessionTableColumnSettings,
  saveSessionPrefs,
  saveSessionTableColumnSettings,
  sessionPageTitle,
  type LoadErrorState,
  type LoadOptions,
} from './sessions-page-prefs'
import { buildSessionListParams, fetchSessionsOnce } from './sessions-page-data'
import { SortIcon } from '@/components/sessions/SortIcon'
export default function SessionsPage() {
  const initialPrefs = useMemo(() => loadSessionPrefs(), [])
  const [searchParams, setSearchParams] = useSearchParams()
  const [selectedNode, setSelectedNode] = useState<string | null>(
    () => normalizeStoredNode(searchParams.get('node')) ?? initialPrefs.node
  )
  const [nodes, setNodes] = useState<NodeSummary[]>([])
  const [sessions, setSessions] = useState<SessionSummary[]>([])
  const [remoteTotal, setRemoteTotal] = useState(0)
  const [loading, setLoading] = useState(true)
  const [refreshing, setRefreshing] = useState(false)
  const [search, setSearch] = useState(initialPrefs.search)
  const [statusFilter, setStatusFilter] = useState<SessionStatusFilter>(initialPrefs.statusFilter)
  const [groupBy, setGroupBy] = useState<GroupBy>(initialPrefs.groupBy)
  const [sortField, setSortField] = useState<SessionSortField>(initialPrefs.sortField)
  const [sortOrder, setSortOrder] = useState<SortOrder>(initialPrefs.sortOrder)
  const [pageSize, setPageSize] = useState<number>(initialPrefs.pageSize)
  const [page, setPage] = useState(0)
  const [showNewSession, setShowNewSession] = useState(false)
  const [rerunSession, setRerunSession] = useState<SessionSummary | null>(null)
  const [editingSession, setEditingSession] = useState<SessionSummary | null>(null)
  const [deletingSession, setDeletingSession] = useState<SessionSummary | null>(null)
  const [enteringIds, setEnteringIds] = useState<Set<string>>(new Set())
  const [notificationRequestIds, setNotificationRequestIds] = useState<Set<string>>(new Set())
  const [showFilters, setShowFilters] = useState(false)
  const [pushState, setPushState] = useState<PushSetupState>('idle')
  const [loadError, setLoadError] = useState<LoadErrorState | null>(null)
  // Pinned live sessions (browser-local only, most recently pinned first).
  const [pinnedKeys, setPinnedKeys] = useState<string[]>(loadPinnedSessionKeys)
  const pinnedKeySet = useMemo(() => new Set(pinnedKeys), [pinnedKeys])
  const [tableColumnSettings, setTableColumnSettings] = useState(loadSessionTableColumnSettings)
  const tableColumnSizes = tableColumnSettings.sizes
  const tableColumnOrder = tableColumnSettings.order
  const orderedTableColumns = useMemo(
    () => getOrderedSessionTableColumns(tableColumnOrder),
    [tableColumnOrder]
  )

  const enterAnimTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const delayedReloadTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  // Optimistic `stopping` claims keyed by session id, re-applied whenever a
  // server refresh would otherwise wipe them with a stale `running`.
  const pendingTerminationRef = useRef(new Map<string, SessionStatus>())
  const isMounted = useRef(true)
  const prevIdsRef = useRef<Set<string>>(new Set())
  const hasLoadedRef = useRef(false)
  const requestVersionRef = useRef(0)
  const selectedNodeRef = useRef(selectedNode)
  const sseStatus = useSseConnectionState()

  // Ids currently rendered on the page (the SSE handler must not invent rows).
  const loadedSessionIds = useMemo(() => new Set(sessions.map((session) => session.id)), [sessions])

  const applySessionItems = useCallback((items: SessionSummary[]) => {
    ingestSessionSummaries(items)
    setSessions(applyPendingTerminations(items, pendingTerminationRef.current))
  }, [])

  const applyLoadedSessionSnapshot = useCallback(
    (items: SessionSummary[]) => {
      const filteredItems = applyPendingTerminations(
        filterSessionsByStatus(items, statusFilter),
        pendingTerminationRef.current
      )
      const itemsById = new Map(filteredItems.map((session) => [session.id, session]))
      setSessions((prev) => {
        const next = prev
          .filter((session) => itemsById.has(session.id))
          .map((session) => itemsById.get(session.id) ?? session)
        if (
          next.length === prev.length &&
          next.every((session, index) => session === prev[index])
        ) {
          return prev
        }
        return next
      })
    },
    [statusFilter]
  )

  const replaceLoadedSession = useCallback((session: SessionSummary) => {
    const effective = applyPendingTermination(session, pendingTerminationRef.current)
    setSessions((prev) => {
      const index = prev.findIndex((item) => item.id === effective.id)
      if (index === -1) return prev
      const next = prev.slice()
      next[index] = effective
      return next
    })
  }, [])

  const removeLoadedSession = useCallback((sessionId: string) => {
    pendingTerminationRef.current.delete(sessionId)
    setSessions((prev) => {
      const index = prev.findIndex((item) => item.id === sessionId)
      if (index === -1) return prev
      return prev.filter((item) => item.id !== sessionId)
    })
  }, [])

  const setLoadedSessionNotifications = useCallback((sessionId: string, enabled: boolean) => {
    setSessions((prev) => {
      const index = prev.findIndex((item) => item.id === sessionId)
      if (index === -1 || prev[index]?.notifications_enabled === enabled) return prev
      const next = prev.slice()
      next[index] = { ...next[index], notifications_enabled: enabled }
      return next
    })
  }, [])

  const setLoadedSessionStatus = useCallback((sessionId: string, status: SessionStatus) => {
    setSessions((prev) => withSessionStatus(prev, sessionId, status))
  }, [])

  const loadLocal = useCallback(
    async (opts?: LoadOptions) => {
      if (selectedNode || selectedNodeRef.current !== selectedNode) return

      const requestVersion = ++requestVersionRef.current
      const isCurrent = () =>
        isMounted.current &&
        requestVersionRef.current === requestVersion &&
        selectedNodeRef.current === selectedNode

      const shouldShowSkeleton = !opts?.background && !hasLoadedRef.current
      if (shouldShowSkeleton || !opts || opts?.background === false) setLoading(true)
      else setRefreshing(true)

      try {
        const params: ListParams = buildSessionListParams({
          search,
          statusFilter,
          page,
          pageSize,
          sortField,
          sortOrder,
        })
        const res = await fetchSessionsOnce(params)
        if (!isCurrent()) return

        hasLoadedRef.current = true
        applySessionItems(res.items)
        setRemoteTotal(res.total)
      } catch (error) {
        if (isCurrent() && (!opts?.background || opts?.reportError)) {
          setLoadError({
            title: 'Unable to load sessions',
            message: getErrorMessage(error, 'Failed to load local sessions.'),
          })
        }
      } finally {
        if (isCurrent()) {
          setLoading(false)
          setRefreshing(false)
        }
      }
    },
    [applySessionItems, page, search, selectedNode, sortField, sortOrder, statusFilter, pageSize]
  )

  const loadRemote = useCallback(
    async (opts?: LoadOptions) => {
      if (!selectedNode || selectedNodeRef.current !== selectedNode) return

      const requestVersion = ++requestVersionRef.current
      const isCurrent = () =>
        isMounted.current &&
        requestVersionRef.current === requestVersion &&
        selectedNodeRef.current === selectedNode

      const shouldShowSkeleton = !opts?.background && !hasLoadedRef.current
      if (shouldShowSkeleton) setLoading(true)
      else setRefreshing(true)

      try {
        const params: ListParams = buildSessionListParams({
          search,
          statusFilter,
          page,
          pageSize,
          sortField,
          sortOrder,
          node: selectedNode,
        })
        const res = await fetchSessionsOnce(params)
        if (!isCurrent()) return

        hasLoadedRef.current = true
        applySessionItems(res.items)
        setRemoteTotal(res.total)
      } catch (error) {
        if (isCurrent() && (!opts?.background || opts?.reportError)) {
          setLoadError({
            title: 'Unable to load sessions',
            message: getErrorMessage(error, 'Failed to load remote sessions.'),
          })
        }
      } finally {
        if (isCurrent()) {
          if (shouldShowSkeleton) setLoading(false)
          setRefreshing(false)
        }
      }
    },
    [applySessionItems, page, search, selectedNode, sortField, sortOrder, statusFilter, pageSize]
  )

  // The primary's SSE carries session_updated events for every connected
  // node. Re-pull summaries for the currently-selected remote node on demand:
  // a) when the page mounts, b) when stream_ready lands, and c) when the
  // user-visible filter changes. Activity now arrives via session_activity
  // SSE events, so a polling interval is no longer necessary.
  useEffect(() => {
    if (!selectedNode) return
    let stopped = false
    const tick = async () => {
      if (stopped || !isMounted.current || selectedNodeRef.current !== selectedNode) return
      try {
        const params: ListParams = buildSessionListParams({
          search,
          statusFilter,
          page,
          pageSize,
          sortField,
          sortOrder,
          node: selectedNode,
        })
        const res = await fetchSessions(params)
        if (stopped || !isMounted.current || selectedNodeRef.current !== selectedNode) return
        ingestSessionSummaries(res.items)
      } catch {
        /* swallow */
      }
    }
    void tick()
    return () => {
      stopped = true
    }
  }, [page, search, selectedNode, sortField, sortOrder, statusFilter, pageSize])

  const reloadSessions = useCallback(
    async (opts?: LoadOptions) => {
      void fetchNodes()
        .then((nextNodes) => {
          if (isMounted.current) setNodes(nextNodes)
        })
        .catch((error) => {
          if (isMounted.current && (!opts?.background || opts?.reportError)) {
            setLoadError({
              title: 'Unable to load nodes',
              message: getErrorMessage(error, 'Failed to refresh connected nodes.'),
            })
          }
        })
      if (selectedNode) {
        await loadRemote(opts)
        return
      }
      await loadLocal(opts)
    },
    [loadLocal, loadRemote, selectedNode]
  )

  const scheduleDelayedReload = useCallback(() => {
    if (delayedReloadTimerRef.current) return
    delayedReloadTimerRef.current = setTimeout(() => {
      delayedReloadTimerRef.current = null
      if (!isMounted.current) return
      void reloadSessions({ background: true })
    }, 5_000)
  }, [reloadSessions])

  useEffect(() => {
    const nextIds = new Set(sessions.map((s) => s.id))
    const prevIds = prevIdsRef.current
    if (prevIds.size > 0) {
      const added = sessions.map((s) => s.id).filter((id) => !prevIds.has(id))
      if (added.length > 0) {
        const addedSet = new Set(added)
        setEnteringIds(addedSet)
        if (enterAnimTimerRef.current) clearTimeout(enterAnimTimerRef.current)
        enterAnimTimerRef.current = setTimeout(() => setEnteringIds(new Set()), 280)
      }
    }
    prevIdsRef.current = nextIds
  }, [sessions])

  useEffect(() => {
    fetchNodes()
      .then(setNodes)
      .catch(() => {})
  }, [])

  useEffect(() => {
    isMounted.current = true
    return () => {
      isMounted.current = false
    }
  }, [])

  useEffect(() => {
    saveSessionPrefs({
      search,
      statusFilter,
      groupBy,
      node: selectedNode,
      sortField,
      sortOrder,
      pageSize,
    })
  }, [search, selectedNode, statusFilter, groupBy, sortField, sortOrder, pageSize])

  useEffect(() => {
    saveSessionTableColumnSettings(tableColumnSettings)
  }, [tableColumnSettings])

  useEffect(() => {
    savePinnedSessionKeys(pinnedKeys)
  }, [pinnedKeys])

  useEffect(() => {
    if (!selectedNode) {
      // Cancel a queued load if the filters/node change before it runs.
      let cancelled = false
      queueMicrotask(() => {
        if (!cancelled) void loadLocal()
      })
      return () => {
        cancelled = true
        requestVersionRef.current += 1
      }
    }
  }, [loadLocal, selectedNode])

  useEffect(() => {
    if (selectedNode) {
      let cancelled = false
      queueMicrotask(() => {
        if (!cancelled) void loadRemote()
      })
      return () => {
        cancelled = true
        requestVersionRef.current += 1
      }
    }
  }, [loadRemote, selectedNode])

  useEffect(() => {
    void syncPushSubscription(false)
      .then((state) => {
        if (isMounted.current) setPushState(state)
      })
      .catch(() => {
        if (isMounted.current) setPushState('idle')
      })
  }, [])

  // ── Live events ───────────────────────────────────────────────────────────
  // One subscription for the whole mount. Context and handlers are rebuilt with
  // their inputs and handed to that single subscription through a ref, so a
  // search keystroke, page flip or sort change no longer tears the subscription
  // down — that churned the shared event store's retain count and cancelled
  // in-flight delayed reloads.
  const sseContext = useMemo<SessionPageEventContext>(
    () => ({
      selectedNode,
      statusFilter,
      loadedSessionIds,
      pushSubscribed: pushState === 'subscribed',
    }),
    [loadedSessionIds, pushState, selectedNode, statusFilter]
  )

  const sseHandlers = useMemo<SessionPageEventHandlers>(
    () => ({
      applySnapshot: applyLoadedSessionSnapshot,
      replaceLoadedSession,
      removeLoadedSession,
      removePinnedKey: (pinKey) => setPinnedKeys((prev) => prev.filter((key) => key !== pinKey)),
      reloadSessions: (opts) => void reloadSessions(opts),
      scheduleDelayedReload,
      showNotification: (data) => void showSessionNotification(data),
    }),
    [
      applyLoadedSessionSnapshot,
      removeLoadedSession,
      replaceLoadedSession,
      reloadSessions,
      scheduleDelayedReload,
    ]
  )

  // Synced by an effect declared before the subscription effect, so the first
  // event of a mount already sees the committed values.
  const sseRoutingRef = useRef({ context: sseContext, handlers: sseHandlers })

  useEffect(() => {
    sseRoutingRef.current = { context: sseContext, handlers: sseHandlers }
  }, [sseContext, sseHandlers])

  useEffect(() => {
    const cleanup = subscribeSessionEvents((ev) => {
      handleSessionPageEvent(ev, sseRoutingRef.current.context, sseRoutingRef.current.handlers)
    })
    return () => {
      cleanup()
      if (enterAnimTimerRef.current) clearTimeout(enterAnimTimerRef.current)
      if (delayedReloadTimerRef.current) clearTimeout(delayedReloadTimerRef.current)
    }
  }, [])

  // Display order: pinned live sessions first (most recently pinned topmost),
  // then — for the default Created At sort — active sessions before finished
  // ones (same rule as the TUI's "active first" strategy). An explicit user
  // sort column keeps the server order below the pinned rows.
  const pagedSessions = useMemo(
    () =>
      orderSessionPage(sessions, {
        pinnedKeys,
        node: selectedNode,
        sortField,
        sortOrder,
      }),
    [sessions, pinnedKeys, selectedNode, sortField, sortOrder]
  )

  const total = remoteTotal

  // Clamp the page when the filtered total shrinks (e.g. after a search).
  // Render-time adjustment is the React-recommended alternative to a
  // setState-in-effect cascade.
  const [prevTotal, setPrevTotal] = useState(total)
  if (prevTotal !== total) {
    setPrevTotal(total)
    const lastPage = Math.max(Math.ceil(total / pageSize) - 1, 0)
    setPage((prev) => Math.min(prev, lastPage))
  }

  const grouped = useMemo<Array<{ key: string; items: SessionSummary[] }>>(() => {
    if (groupBy === 'none') return [{ key: '', items: pagedSessions }]
    if (groupBy === 'cwd') {
      const map = new Map<string, { label: string; items: SessionSummary[] }>()
      for (const s of pagedSessions) {
        const trimmed = s.cwd?.trim() ?? ''
        const label = trimmed ? normalizeCwdPath(trimmed) : '(no cwd)'
        const k = label.toLowerCase()
        if (!map.has(k)) map.set(k, { label, items: [] })
        map.get(k)!.items.push(s)
      }
      return Array.from(map.values()).map(({ label, items }) => ({ key: label, items }))
    }
    if (groupBy === 'tag') {
      const map = new Map<string, SessionSummary[]>()
      for (const s of pagedSessions) {
        const tags = s.tags.length > 0 ? s.tags : ['(untagged)']
        for (const tag of tags) {
          if (!map.has(tag)) map.set(tag, [])
          map.get(tag)!.push(s)
        }
      }
      return Array.from(map.entries()).map(([key, items]) => ({ key, items }))
    }
    const map = new Map<string, SessionSummary[]>()
    for (const s of pagedSessions) {
      const k = agentName(s.command)
      if (!map.has(k)) map.set(k, [])
      map.get(k)!.push(s)
    }
    return Array.from(map.entries()).map(([key, items]) => ({ key, items }))
  }, [groupBy, pagedSessions])

  const handleRunAgain = useCallback((session: SessionSummary) => {
    setRerunSession(session)
    setShowNewSession(true)
  }, [])

  const handleEditSession = useCallback((session: SessionSummary) => {
    setEditingSession(session)
  }, [])

  function isSessionPinned(session: SessionSummary): boolean {
    return pinnedKeySet.has(sessionPinKey(session.id, selectedNode))
  }

  const handleTogglePin = useCallback(
    (session: SessionSummary) => {
      if (!sessionIsPinnable(session)) return
      const key = sessionPinKey(session.id, selectedNode)
      setPinnedKeys((prev) =>
        prev.includes(key) ? prev.filter((entry) => entry !== key) : [key, ...prev]
      )
    },
    [selectedNode]
  )

  function handleNodeChange(node: string | null) {
    if (node === selectedNode) return
    selectedNodeRef.current = node
    requestVersionRef.current += 1
    if (delayedReloadTimerRef.current) clearTimeout(delayedReloadTimerRef.current)
    delayedReloadTimerRef.current = null
    hasLoadedRef.current = false
    prevIdsRef.current = new Set()
    pendingTerminationRef.current.clear()
    setSessions([])
    setRemoteTotal(0)
    setLoading(true)
    setRefreshing(false)
    setSelectedNode(node)
    setPage(0)
    setSearchParams(
      (prev) => {
        const next = new URLSearchParams(prev)
        if (node) next.set('node', node)
        else next.delete('node')
        return next
      },
      { replace: true }
    )
  }

  // Pull-to-refresh and node swipe: one continuous touch, drawn by writing
  // styles directly rather than re-rendering the list on every move.
  const {
    onTouchStart: handleMobileTouchStart,
    onTouchMove: handleMobileTouchMove,
    onTouchEnd: handleMobileTouchEnd,
    onTouchCancel: handleMobileTouchCancel,
    noteNonTouchPointer: noteNonTouchGesture,
    shouldSuppressClick: gestureSwallowsClick,
    contentRef: mobileSwipeContentRef,
    indicatorRef: mobilePullIndicatorRef,
    labelRef: mobilePullLabelRef,
  } = useSessionListGestures({
    selectedNode,
    nodes,
    loading,
    refreshing,
    onNodeChange: handleNodeChange,
    onRefresh: () => reloadSessions({ background: true, reportError: true }),
  })

  function handleDeleted(id: string) {
    removeLoadedSession(id)
    setPinnedKeys((prev) => prev.filter((key) => key !== sessionPinKey(id, selectedNode)))
    void reloadSessions({ background: true })
  }

  // Stop/Kill paint their result optimistically. The daemon moves the runtime
  // to `stopping` immediately but only publishes a summary once the whole
  // grace window has elapsed, so an authoritative-only row would keep reading
  // `running` for up to `stop_grace_seconds` after the user confirmed.
  const runTermination = useCallback(
    async (action: SessionTermination, session: SessionSummary) => {
      const pending = withPendingTermination(session)
      // Already stopping or finished: re-sending would restart the daemon's
      // escalation schedule and push the kill deadline further out.
      if (!pending) return

      const previousStatus = session.status
      pendingTerminationRef.current.set(session.id, pending.status)
      setLoadedSessionStatus(session.id, pending.status)

      try {
        if (action === 'stop') {
          await stopSession(session.id, undefined, selectedNode ?? undefined)
        } else {
          await killSession(session.id, selectedNode ?? undefined)
        }
      } catch (error) {
        // Roll back only while the row still shows the status we claimed; a
        // `session_updated` that arrived meanwhile stays authoritative.
        pendingTerminationRef.current.delete(session.id)
        setSessions((prev) => revertSessionStatus(prev, session.id, pending.status, previousStatus))
        setLoadError({
          title: action === 'stop' ? 'Failed to stop session' : 'Failed to kill session',
          message: getErrorMessage(error, `Failed to ${action} session.`),
        })
        return
      }

      // A remote node does not stream its own summaries here, so re-pull the
      // page once the request settles. Local rows reconcile through the shared
      // `session_updated` event.
      if (selectedNode) void loadRemote()
    },
    [loadRemote, selectedNode, setLoadedSessionStatus]
  )

  const handleStop = useCallback(
    (session: SessionSummary) => runTermination('stop', session),
    [runTermination]
  )

  const handleKill = useCallback(
    (session: SessionSummary) => runTermination('kill', session),
    [runTermination]
  )

  const handleToggleNotifications = useCallback(
    async (session: SessionSummary) => {
      const isRunning =
        session.status === 'running' ||
        session.status === 'stopping' ||
        session.status === 'created'
      if (!isRunning) return

      const nextEnabled = !session.notifications_enabled
      setNotificationRequestIds((prev) => new Set(prev).add(session.id))
      setLoadedSessionNotifications(session.id, nextEnabled)

      try {
        await setSessionNotifications(session.id, nextEnabled, selectedNode ?? undefined)
      } catch (error) {
        setLoadedSessionNotifications(session.id, session.notifications_enabled)
        setLoadError({
          title: nextEnabled ? 'Failed to enable notifications' : 'Failed to disable notifications',
          message: getErrorMessage(error, 'Failed to update session notifications.'),
        })
      } finally {
        setNotificationRequestIds((prev) => {
          const next = new Set(prev)
          next.delete(session.id)
          return next
        })
      }
    },
    [selectedNode, setLoadedSessionNotifications]
  )

  const totalPages = Math.ceil(total / pageSize)

  const pageTitle = sessionPageTitle(selectedNode)

  function handleSort(field: SessionSortField) {
    let nextSortField = sortField
    let nextSortOrder: SortOrder
    if (field === sortField) {
      nextSortOrder = sortOrder === SortOrder.Asc ? SortOrder.Desc : SortOrder.Asc
      setSortOrder(nextSortOrder)
    } else {
      nextSortField = field
      nextSortOrder = SortOrder.Asc
      setSortField(nextSortField)
      setSortOrder(nextSortOrder)
    }
    saveSessionPrefs({
      search,
      statusFilter,
      groupBy,
      node: selectedNode,
      sortField: nextSortField,
      sortOrder: nextSortOrder,
      pageSize,
    })
    setPage(0)
  }

  const {
    beginResize: beginColumnResize,
    updateResize: updateColumnResize,
    endResize: endColumnResize,
    beginReorder: beginColumnReorder,
    moveBefore: moveColumnBefore,
    dropBefore: dropColumnBefore,
    endReorder: endColumnReorder,
  } = useSessionTableColumns(tableColumnSettings, setTableColumnSettings)

  const hasActiveFilters =
    search !== '' ||
    statusFilter !== 'all' ||
    groupBy !== 'none' ||
    sortField !== SessionSortField.CreatedAt ||
    sortOrder !== SortOrder.Desc

  const statusChips: { label: string; value: SessionStatusFilter }[] = [
    { label: 'All status', value: 'all' },
    { label: 'Running', value: 'running' },
    { label: 'Stopped', value: 'stopped' },
    { label: 'Killed', value: 'killed' },
    { label: 'Failed', value: 'failed' },
    { label: 'Stopping', value: 'stopping' },
  ]

  const pushEnabled = pushState === 'subscribed'
  const pushButtonLabel = pushEnabled
    ? 'Push On'
    : pushState === 'denied'
      ? 'Push Blocked'
      : pushState === 'unsupported'
        ? 'Push Unsupported'
        : pushState === 'unconfigured'
          ? 'Push Unconfigured'
          : 'Enable Push'

  async function handleEnablePush() {
    const next = await syncPushSubscription(true).catch(() => null)
    if (!next) return
    setPushState(next)
  }

  async function handleTogglePush() {
    if (pushEnabled) {
      const next = await disablePushNotifications().catch(() => null)
      if (!next) return
      setPushState(next)
      return
    }
    await handleEnablePush()
  }

  const statusFilterView = (
    <Select
      value={statusFilter}
      onValueChange={(v) => {
        if (isSessionStatusFilter(v)) {
          setStatusFilter(v)
          setPage(0)
        }
      }}
    >
      <SelectTrigger className="flex-1 sm:flex-0 h-8 text-xs">
        <SelectValue placeholder="All statuses" />
      </SelectTrigger>
      <SelectContent>
        {statusChips.map((chip) => (
          <SelectItem key={chip.value} value={chip.value}>
            {chip.label}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  )
  const tableWidth = getSessionTableWidth(tableColumnSizes)

  return (
    <TooltipProvider>
      <div className="flex flex-col h-full bg-[hsl(var(--background))] text-[hsl(var(--foreground))]">
        <Dialog
          open={loadError !== null}
          onOpenChange={(open) => {
            if (!open) setLoadError(null)
          }}
        >
          <DialogContent className="max-w-sm">
            <DialogHeader>
              <DialogTitle>{loadError?.title ?? 'Error'}</DialogTitle>
            </DialogHeader>
            <p className="text-sm text-[hsl(var(--muted-foreground))]">
              {loadError?.message ?? 'Something went wrong.'}
            </p>
            <div className="flex justify-end pt-1">
              <Button size="sm" onClick={() => setLoadError(null)}>
                Close
              </Button>
            </div>
          </DialogContent>
        </Dialog>

        {/* ── Header ── */}
        <header className="border-b border-[hsl(var(--border))] bg-[hsl(var(--background))]/95 sticky top-0 z-30 backdrop-blur">
          {/* Mobile row */}
          <div className="flex flex-nowrap items-center gap-2 px-3 py-2 md:hidden">
            <div
              className="flex items-center gap-2 text-[hsl(var(--primary))] font-bold text-lg cursor-pointer min-w-0"
              onClick={() => void reloadSessions({ background: false })}
            >
              <Logo />
              {nodes.length === 0 && <span className="truncate">{pageTitle}</span>}
            </div>
            {nodes.length > 0 && (
              <NodeSelector
                nodes={nodes}
                selected={selectedNode}
                onChange={handleNodeChange}
                className="h-7 w-auto min-w-0 max-w-[40vw] grow-0 basis-auto text-xs normal-case"
              />
            )}
            <div className="flex-1 min-w-0" />
            <Button
              variant="ghost"
              size="icon"
              className={
                hasActiveFilters
                  ? 'text-[hsl(var(--primary))] bg-[hsl(var(--primary))]/10 relative'
                  : 'relative'
              }
              onClick={() => setShowFilters((v) => !v)}
              aria-label="Toggle filters"
            >
              <MixerHorizontalIcon className="h-4 w-4" />
              {hasActiveFilters && (
                <span className="absolute top-1 right-1 w-1.5 h-1.5 rounded-full bg-[hsl(var(--primary))]" />
              )}
            </Button>
            <Button asChild variant="ghost" size="icon">
              <a href="/apps" aria-label="Apps">
                <GridIcon className="h-4 w-4" />
              </a>
            </Button>
            <Button
              variant={pushEnabled ? 'link' : 'ghost'}
              size="icon"
              onClick={() => void handleTogglePush()}
              disabled={pushState === 'unsupported' || pushState === 'unconfigured'}
            >
              <BellIcon className="h-4 w-4" />
            </Button>
            <Button size="icon" onClick={() => setShowNewSession(true)} aria-label="New session">
              <PlusIcon className="h-4 w-4" />
            </Button>
          </div>

          {/* Mobile filter drawer */}
          <div
            className={`md:hidden overflow-hidden transition-all duration-200 ${showFilters ? 'max-h-64 opacity-100' : 'max-h-0 opacity-0'}`}
          >
            <div className="px-3 pb-3 mt-1 flex flex-col gap-2">
              <div className="relative">
                <Input
                  className={search ? 'pr-8' : undefined}
                  placeholder="Search cmd: cwd: title: tag:"
                  aria-label="Search sessions by id, title, command, or working directory"
                  value={search}
                  onChange={(e) => {
                    setSearch(e.target.value)
                    setPage(0)
                  }}
                />
                {search && (
                  <button
                    type="button"
                    aria-label="Clear search"
                    className="absolute right-2 top-1/2 -translate-y-1/2 text-[hsl(var(--muted-foreground))] transition-colors hover:text-[hsl(var(--foreground))]"
                    onClick={() => {
                      setSearch('')
                      setPage(0)
                    }}
                  >
                    <Cross2Icon className="h-3.5 w-3.5" />
                  </button>
                )}
              </div>
              <div className="flex gap-2">
                <Select value={groupBy} onValueChange={(v) => setGroupBy(v as GroupBy)}>
                  <SelectTrigger className="min-w-0 flex-1 h-8 text-xs">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="none">No grouping</SelectItem>
                    <SelectItem value="tag">Tag</SelectItem>
                    <SelectItem value="cwd">CWD</SelectItem>
                    <SelectItem value="command">Command</SelectItem>
                  </SelectContent>
                </Select>
                {statusFilterView}
              </div>
              <div className="flex gap-2">
                <Select
                  value={sortField}
                  onValueChange={(v) => {
                    const nextSortField = v as SessionSortField
                    setSortField(nextSortField)
                    saveSessionPrefs({
                      search,
                      statusFilter,
                      groupBy,
                      node: selectedNode,
                      sortField: nextSortField,
                      sortOrder,
                      pageSize,
                    })
                    setPage(0)
                  }}
                >
                  <SelectTrigger className="flex-1 h-8 text-xs">
                    <SelectValue placeholder="Sort by" />
                  </SelectTrigger>
                  <SelectContent>
                    {SORT_OPTIONS.map((option) => (
                      <SelectItem key={option.value} value={option.value}>
                        {`Sort by ${option.label}`}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <Select
                  value={sortOrder}
                  onValueChange={(v) => {
                    const nextSortOrder = v as SortOrder
                    setSortOrder(nextSortOrder)
                    saveSessionPrefs({
                      search,
                      statusFilter,
                      groupBy,
                      node: selectedNode,
                      sortField,
                      sortOrder: nextSortOrder,
                      pageSize,
                    })
                    setPage(0)
                  }}
                >
                  <SelectTrigger className="flex-1 h-8 text-xs">
                    <SelectValue placeholder="Order" />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value={SortOrder.Desc}>Descending</SelectItem>
                    <SelectItem value={SortOrder.Asc}>Ascending</SelectItem>
                  </SelectContent>
                </Select>
              </div>
            </div>
          </div>

          {/* Desktop row */}
          <div className="hidden md:flex flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2.5">
            <div
              className="flex items-center gap-1 text-[hsl(var(--primary))] font-bold text-lg cursor-pointer"
              onClick={() => void reloadSessions({ background: false })}
            >
              <Logo />
              <span>Open Relay</span>
            </div>

            <div className="relative w-48">
              <Input
                className={search ? 'h-8 w-full pr-8 text-sm' : 'h-8 w-full text-sm'}
                placeholder="Search cmd: cwd: title: tag:"
                aria-label="Search sessions by id, title, command, or working directory"
                value={search}
                onChange={(e) => {
                  setSearch(e.target.value)
                  setPage(0)
                }}
              />
              {search && (
                <button
                  type="button"
                  aria-label="Clear search"
                  className="absolute right-2 top-1/2 -translate-y-1/2 text-[hsl(var(--muted-foreground))] transition-colors hover:text-[hsl(var(--foreground))]"
                  onClick={() => {
                    setSearch('')
                    setPage(0)
                  }}
                >
                  <Cross2Icon className="h-3.5 w-3.5" />
                </button>
              )}
            </div>

            <Select value={groupBy} onValueChange={(v) => setGroupBy(v as GroupBy)}>
              <SelectTrigger className="flex-0 h-8 text-sm">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="none">No grouping</SelectItem>
                <SelectItem value="tag">Tag</SelectItem>
                <SelectItem value="cwd">CWD</SelectItem>
                <SelectItem value="command">Command</SelectItem>
              </SelectContent>
            </Select>

            {/* Status filter (responsive) */}
            {statusFilterView}

            <NodeSelector nodes={nodes} selected={selectedNode} onChange={handleNodeChange} />

            <div className="flex-1" />

            <Button
              size="sm"
              variant="ghost"
              onClick={() => void reloadSessions({ background: false })}
              disabled={loading || refreshing}
            >
              <ReloadIcon className="h-4 w-4" />
            </Button>

            <Button asChild size="sm" variant="ghost">
              <a href="/apps">
                <GridIcon className="h-4 w-4" />
              </a>
            </Button>

            <Button
              size="sm"
              variant={pushEnabled ? 'link' : 'ghost'}
              onClick={() => void handleTogglePush()}
              disabled={pushState === 'unsupported' || pushState === 'unconfigured'}
            >
              <BellIcon className="h-4 w-4" />
              <span className="hidden xl:inline">{pushButtonLabel}</span>
            </Button>

            <Button size="sm" onClick={() => setShowNewSession(true)}>
              <PlayIcon className="h-4 w-4" />
              <span className="hidden xl:inline">New</span>
            </Button>
          </div>
        </header>

        {/* ── Mobile list ── */}
        <div
          className="flex-1 overflow-y-auto overflow-x-clip md:hidden"
          data-testid="mobile-session-list"
          onPointerDownCapture={(event) => {
            if (event.pointerType !== 'touch') noteNonTouchGesture()
          }}
          onTouchStart={handleMobileTouchStart}
          onTouchMove={handleMobileTouchMove}
          onTouchEnd={handleMobileTouchEnd}
          onTouchCancel={handleMobileTouchCancel}
          onClickCapture={(event) => {
            if (gestureSwallowsClick()) {
              event.preventDefault()
              event.stopPropagation()
            }
          }}
        >
          <div className="relative">
            <div
              ref={mobilePullIndicatorRef}
              data-testid="mobile-pull-indicator"
              role="status"
              aria-live="polite"
              aria-hidden="true"
              className="pointer-events-none absolute inset-x-0 top-0 z-10 flex h-12 items-center justify-center gap-2 text-xs font-medium text-[hsl(var(--muted-foreground))] opacity-0"
            >
              <span className="inline-flex items-center gap-2 rounded-full border border-[hsl(var(--border))] bg-[hsl(var(--card))]/95 px-3 py-1.5 shadow-sm">
                <ReloadIcon className="h-4 w-4" />
                <span ref={mobilePullLabelRef}>Pull to refresh</span>
              </span>
            </div>
            <div ref={mobileSwipeContentRef} data-testid="mobile-session-content">
              {loading &&
                sessions.length === 0 &&
                Array.from({ length: 5 }).map((_, i) => <SkeletonCard key={i} />)}
              {!loading && sessions.length === 0 && (
                <SessionsEmptyState
                  selectedNode={selectedNode}
                  onNewSession={() => setShowNewSession(true)}
                />
              )}
              {!loading && sessions.length > 0 && (
                <div className="pb-4">
                  {grouped.map(({ key, items }) => (
                    <div key={key || '__flat__'}>
                      {groupBy !== 'none' && key && (
                        <div className="flex flex-nowrap gap-1 items-center px-2 py-1 text-xs text-[hsl(var(--muted-foreground))] font-medium bg-[hsl(var(--primary))]/10">
                          <CaretDownIcon className="h-4 w-4 shrink-0" />
                          <GroupHeaderLabel groupBy={groupBy} keyLabel={key} items={items} />
                        </div>
                      )}
                      {items.map((s) => (
                        <SessionCard
                          key={s.id}
                          session={s}
                          animateIn={enteringIds.has(s.id)}
                          pinned={isSessionPinned(s)}
                          onStop={handleStop}
                          onKill={handleKill}
                          onToggleNotifications={handleToggleNotifications}
                          onTogglePin={handleTogglePin}
                          onRunAgain={handleRunAgain}
                          onEditSession={handleEditSession}
                          onRequestDelete={setDeletingSession}
                          notificationsPending={notificationRequestIds.has(s.id)}
                          node={selectedNode ?? undefined}
                          showCwd={groupBy !== 'cwd'}
                        />
                      ))}
                    </div>
                  ))}
                </div>
              )}
            </div>
          </div>
        </div>

        {/* ── Desktop table ── */}
        <div className="flex-1 h-full shrink overflow-x-auto hidden md:block">
          {loading && sessions.length === 0 && (
            <Table className="w-full border-collapse table-fixed">
              <TableBody>
                {Array.from({ length: 8 }).map((_, i) => (
                  <SkeletonRow key={i} />
                ))}
              </TableBody>
            </Table>
          )}
          {!loading && sessions.length === 0 && (
            <SessionsEmptyState
              selectedNode={selectedNode}
              onNewSession={() => setShowNewSession(true)}
            />
          )}
          {!loading && sessions.length > 0 && (
            <Table
              className="w-full border-collapse table-fixed"
              style={{ minWidth: `${tableWidth}px` }}
            >
              <colgroup>
                {orderedTableColumns.map((column) => (
                  <col key={column.key} style={{ width: `${tableColumnSizes[column.key]}px` }} />
                ))}
              </colgroup>
              <TableHeader>
                <TableRow>
                  {orderedTableColumns.map((col) => {
                    const sortableField = col.sortField
                    return (
                      <TableHead
                        key={col.key}
                        onDragOver={(event) => moveColumnBefore(col.key, event)}
                        onDrop={(event) => dropColumnBefore(col.key, event)}
                        className={`relative px-3 py-1 text-left text-xs font-medium tracking-wide border-b border-[hsl(var(--border))] bg-[hsl(var(--background))] sticky z-20 select-none whitespace-nowrap ${
                          sortableField
                            ? 'cursor-pointer hover:text-[hsl(var(--foreground))] transition-colors'
                            : 'text-[hsl(var(--muted-foreground))]'
                        } ${sortableField === sortField ? 'text-[hsl(var(--primary))]' : 'text-[hsl(var(--muted-foreground))]'}`}
                        onClick={sortableField ? () => handleSort(sortableField) : undefined}
                      >
                        <span className="inline-flex items-center gap-1">
                          <button
                            type="button"
                            draggable
                            aria-label={`Drag ${col.label} column`}
                            className="inline-flex h-5 w-4 cursor-grab items-center justify-center rounded text-[hsl(var(--muted-foreground))] hover:bg-[hsl(var(--accent))] hover:text-[hsl(var(--foreground))] active:cursor-grabbing"
                            onClick={(event) => event.stopPropagation()}
                            onDragStart={(event) => beginColumnReorder(col.key, event)}
                            onDragEnd={endColumnReorder}
                          >
                            <GridIcon className="h-3 w-3" />
                          </button>
                          <span className="inline-flex min-w-0 items-center gap-1">
                            {col.label}
                            {sortableField && (
                              <SortIcon
                                field={sortableField}
                                sortField={sortField}
                                sortOrder={sortOrder}
                              />
                            )}
                          </span>
                        </span>
                        <span
                          role="separator"
                          aria-orientation="vertical"
                          aria-label={`Resize ${col.label} column`}
                          className="absolute right-0 top-0 z-30 h-full w-2 cursor-col-resize touch-none select-none after:absolute after:right-0 after:top-2 after:h-[calc(100%-1rem)] after:w-px after:bg-[hsl(var(--border))] hover:after:bg-[hsl(var(--primary))]"
                          onClick={(event) => event.stopPropagation()}
                          onPointerDown={(event) => beginColumnResize(col.key, event)}
                          onPointerMove={updateColumnResize}
                          onPointerUp={endColumnResize}
                          onPointerCancel={endColumnResize}
                          onDoubleClick={(event) => {
                            event.stopPropagation()
                            setTableColumnSettings((previous) => ({
                              ...previous,
                              sizes: {
                                ...previous.sizes,
                                [col.key]: col.defaultWidth,
                              },
                            }))
                          }}
                        />
                      </TableHead>
                    )
                  })}
                </TableRow>
              </TableHeader>
              <TableBody>
                {grouped.map(({ key, items }) => (
                  <Fragment key={key || '__flat__'}>
                    {groupBy !== 'none' && key && (
                      <TableRow>
                        <TableCell
                          colSpan={orderedTableColumns.length}
                          className="px-2 py-1 text-xs text-[hsl(var(--muted-foreground))] font-medium bg-[hsl(var(--primary))]/10"
                        >
                          <div className="flex items-center gap-1">
                            <CaretDownIcon className="h-4 w-4" />
                            <GroupHeaderLabel groupBy={groupBy} keyLabel={key} items={items} />
                          </div>
                        </TableCell>
                      </TableRow>
                    )}
                    {items.map((s) => (
                      <SessionRow
                        key={`${s.id}:${s.status}:${s.input_needed ? 'input' : 'normal'}`}
                        session={s}
                        animateIn={enteringIds.has(s.id)}
                        pinned={isSessionPinned(s)}
                        onStop={handleStop}
                        onKill={handleKill}
                        onToggleNotifications={handleToggleNotifications}
                        onTogglePin={handleTogglePin}
                        onRunAgain={handleRunAgain}
                        onEditSession={handleEditSession}
                        onRequestDelete={setDeletingSession}
                        notificationsPending={notificationRequestIds.has(s.id)}
                        node={selectedNode ?? undefined}
                        columns={orderedTableColumns}
                      />
                    ))}
                  </Fragment>
                ))}
              </TableBody>
            </Table>
          )}
        </div>

        {/* ── Meta bar ── */}
        <div className="flex items-center overflow-x-auto gap-2 px-2 py-2 border-t border-[hsl(var(--border))] bg-[hsl(var(--background))]/80 text-sm text-[hsl(var(--muted-foreground))]">
          <SseStatusDot status={sseStatus} />
          {refreshing && !loading && (
            <span className="text-[hsl(var(--muted-foreground))]">Refreshing…</span>
          )}
          <div className="flex-1"></div>
          <Select
            value={String(pageSize)}
            onValueChange={(value) => {
              const next = Number(value)
              if (!Number.isInteger(next) || next <= 0 || next === pageSize) return
              setPageSize(next)
              setPage(0)
            }}
          >
            <SelectTrigger
              aria-label="Sessions per page"
              className="h-7 w-auto shrink-0 min-w-0 px-2 text-xs text-[hsl(var(--muted-foreground))]"
            >
              {pageSize}
            </SelectTrigger>
            <SelectContent>
              {PAGE_SIZE_OPTIONS.map((size) => (
                <SelectItem key={size} value={String(size)}>
                  {size}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          <span className="whitespace-nowrap text-sm tabular-nums">/ {total}</span>
          {totalPages > 1 && (
            <div className="flex items-center gap-0.5">
              <Button
                variant="ghost"
                size="icon"
                disabled={page === 0}
                aria-label="Previous page"
                onClick={() => setPage((p) => p - 1)}
              >
                <ChevronLeftIcon className="h-4 w-4" />
              </Button>
              <span className="whitespace-nowrap px-1 sm:px-2 text-sm tabular-nums">
                {page + 1} / {totalPages}
              </span>
              <Button
                variant="ghost"
                size="icon"
                disabled={page >= totalPages - 1}
                aria-label="Next page"
                onClick={() => setPage((p) => p + 1)}
              >
                <ChevronRightIcon className="h-4 w-4" />
              </Button>
            </div>
          )}
        </div>

        {deletingSession && (
          <SessionDeleteConfirmDialog
            open
            session={sessions.find((item) => item.id === deletingSession.id) ?? deletingSession}
            node={selectedNode ?? undefined}
            onClose={() => setDeletingSession(null)}
            onRemoved={() => handleDeleted(deletingSession.id)}
          />
        )}
        <NewSessionDialog
          open={showNewSession}
          onClose={() => {
            setShowNewSession(false)
            setRerunSession(null)
            void reloadSessions({ background: true })
          }}
          initialValues={rerunSession ? buildNewSessionInitialValues(rerunSession) : undefined}
          node={selectedNode ?? undefined}
        />
        <SessionMetadataDialog
          open={editingSession !== null}
          session={editingSession}
          node={selectedNode ?? undefined}
          onClose={() => setEditingSession(null)}
          onSaved={(session: SessionSummary) => {
            replaceLoadedSession(session)
            setEditingSession(session)
          }}
        />
      </div>
    </TooltipProvider>
  )
}
