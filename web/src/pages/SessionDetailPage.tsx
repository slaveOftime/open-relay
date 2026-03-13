import { useState, useEffect, useRef, useCallback } from 'react'
import { useParams, useSearchParams, useNavigate } from 'react-router-dom'
import type { SessionSummary } from '@/api/types'
import { fetchSession, fetchLogs, stopSession, killSession, AttachSocket } from '@/api/client'
import { formatAge } from '@/utils/format'
import StatusBadge from '@/components/StatusBadge'
import XTerm, { type XTermHandle } from '@/components/XTerm'
import Logo from '@/components/Logo'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { Slider } from '@/components/ui/slider'
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip'
import {
  ChevronLeftIcon,
  ChevronRightIcon,
  Cross2Icon,
  CrossCircledIcon,
  DotsVerticalIcon,
  FileTextIcon,
  Link1Icon,
  LinkNone2Icon,
  PauseIcon,
  PlayIcon,
  ReloadIcon,
  StopIcon,
  TrackNextIcon,
} from '@radix-ui/react-icons'
import { Link } from 'react-router-dom'
import AttachPanel from '@/components/AttachPanel'

function isSessionRunning(session: SessionSummary | null): boolean {
  return session
    ? session.status === 'running' || session.status === 'stopping' || session.status === 'created'
    : false
}

// ── Confirm Action Dialog ────────────────────────────────────────────────────
function ConfirmActionDialog({
  action,
  sessionId,
  onConfirm,
  onClose,
}: {
  action: 'stop' | 'kill' | null
  sessionId: string
  onConfirm: (a: 'stop' | 'kill') => void
  onClose: () => void
}) {
  return (
    <Dialog
      open={action !== null}
      onOpenChange={(open) => {
        if (!open) onClose()
      }}
    >
      <DialogContent className="max-w-sm">
        <DialogHeader>
          <DialogTitle>{action === 'kill' ? 'Kill Session' : 'Stop Session'}</DialogTitle>
        </DialogHeader>
        <p className="text-sm text-[hsl(var(--muted-foreground))]">
          {action === 'kill' ? (
            <>
              Are you sure you want to{' '}
              <span className="text-red-600 dark:text-red-400 font-semibold">kill</span> session{' '}
              <span className="font-mono text-[hsl(var(--foreground))]">
                {sessionId.slice(0, 7)}
              </span>
              ? The process will be terminated immediately.
            </>
          ) : (
            <>
              Are you sure you want to{' '}
              <span className="text-amber-600 dark:text-amber-400 font-semibold">stop</span> session{' '}
              <span className="font-mono text-[hsl(var(--foreground))]">
                {sessionId.slice(0, 7)}
              </span>
              ? A graceful shutdown signal will be sent.
            </>
          )}
        </p>
        <div className="flex justify-end gap-2 pt-1">
          <Button variant="ghost" size="sm" onClick={onClose}>
            Cancel
          </Button>
          <Button
            variant={action === 'kill' ? 'kill' : 'stop'}
            size="sm"
            onClick={() => {
              if (action) onConfirm(action)
              onClose()
            }}
          >
            Yes
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  )
}

// ── Main Page ─────────────────────────────────────────────────────────────────
export default function SessionDetailPage() {
  const { id } = useParams<{ id: string }>()
  const [searchParams, setSearchParams] = useSearchParams()
  const navigate = useNavigate()

  const mode = (searchParams.get('mode') ?? 'logs') as 'attach' | 'logs'
  const node = searchParams.get('node')

  const [session, setSession] = useState<SessionSummary | null>(null)
  const [logLines, setLogLines] = useState<string[]>([])
  const [replayIdx, setReplayIdx] = useState(0)
  const [scrubberMax, setScrubberMax] = useState(0)
  const [wsConnected, setWsConnected] = useState(false)
  const [wsConnecting, setWsConnecting] = useState(false)
  const [wsEverConnected, setWsEverConnected] = useState(false)
  const [wsReconnectKey, setWsReconnectKey] = useState(0)
  const [wsError, setWsError] = useState<string | null>(null)
  const [connectTraceOpen, setConnectTraceOpen] = useState(false)
  const [connectTrace, setConnectTrace] = useState<string[]>([])
  const [exitCode, setExitCode] = useState<number | null | undefined>(undefined)
  const [pendingAction, setPendingAction] = useState<'stop' | 'kill' | null>(null)
  const [isReplaying, setIsReplaying] = useState(false)
  const [isPaused, setIsPaused] = useState(false)
  const [replaySpeed, setReplaySpeed] = useState(1)
  const [totalLines, setTotalLines] = useState(0)
  const [reloadTick, setReloadTick] = useState(0)
  const [isOnline, setIsOnline] = useState(
    typeof navigator === 'undefined' ? true : navigator.onLine
  )

  const termRef = useRef<XTermHandle>(null)
  const socketRef = useRef<AttachSocket | null>(null)
  const wsConnectedRef = useRef(false)
  const wsConnectingRef = useRef(false)
  const modeRef = useRef(mode)
  const replayRafRef = useRef<number | null>(null)
  const logLinesRef = useRef<string[]>([])
  const replaySpeedRef = useRef(1)
  const isPausedRef = useRef(false)
  const isRunningRef = useRef(false)
  const isReplayingRef = useRef(false)
  const totalLinesRef = useRef(0)
  const nextOffsetRef = useRef(0)
  const isFetchingMoreRef = useRef(false)
  const termContainerRef = useRef<HTMLDivElement>(null)
  const isMounted = useRef(true)
  const replayIdxRef = useRef(0)
  const reconnectAttemptRef = useRef(0)
  const reconnectTimerRef = useRef<number | null>(null)
  const pendingReconnectRef = useRef(false)
  const connectAttemptStartedAtRef = useRef(0)
  const outputBufferRef = useRef<Uint8Array[]>([])
  const outputFlushRafRef = useRef<number | null>(null)
  const pendingResetRef = useRef(false)
  const resizeDebounceRef = useRef<number | null>(null)
  const pendingResizeRef = useRef<{ cols: number; rows: number } | null>(null)
  const lastSentResizeRef = useRef<{ cols: number; rows: number } | null>(null)
  const lastReconnectTriggerAtRef = useRef(0)
  const lastWsFrameAtRef = useRef(0)

  const flushTerminalOutput = useCallback(() => {
    outputFlushRafRef.current = null
    const term = termRef.current
    if (!term) {
      outputBufferRef.current = []
      pendingResetRef.current = false
      return
    }
    if (pendingResetRef.current) {
      term.reset()
      pendingResetRef.current = false
    }
    if (outputBufferRef.current.length > 0) {
      const chunks = outputBufferRef.current
      outputBufferRef.current = []
      let total = 0
      for (const c of chunks) total += c.length
      const merged = new Uint8Array(total)
      let off = 0
      for (const c of chunks) {
        merged.set(c, off)
        off += c.length
      }
      term.write(merged)
    }
    term.scrollToBottom()
  }, [])

  const enqueueTerminalOutput = useCallback(
    (chunks: Uint8Array[], opts?: { reset?: boolean }) => {
      if (!isMounted.current) return
      if (opts?.reset) {
        pendingResetRef.current = true
        outputBufferRef.current = []
      }
      if (chunks.length > 0) {
        outputBufferRef.current.push(...chunks)
      }
      if (outputFlushRafRef.current === null) {
        outputFlushRafRef.current = requestAnimationFrame(flushTerminalOutput)
      }
    },
    [flushTerminalOutput]
  )

  const pushConnectTrace = useCallback((step: string) => {
    const ts = new Date().toISOString()
    setConnectTrace((prev) => [`${ts} ${step}`, ...prev].slice(0, 200))
  }, [])

  const requestReconnect = useCallback(
    (source: string, force = false) => {
      if (document.visibilityState !== 'visible') return false
      if (typeof navigator !== 'undefined' && !navigator.onLine) return false

      const now = Date.now()
      const sinceLast = now - lastReconnectTriggerAtRef.current
      const connecting = wsConnectingRef.current
      const connectStartedAt = connectAttemptStartedAtRef.current
      const connectAge = connectStartedAt > 0 ? now - connectStartedAt : Number.POSITIVE_INFINITY
      const stalledConnect = connecting && connectAge > 9000

      if (sinceLast < (force ? 1200 : 2200)) return false
      if (!force && (wsConnectedRef.current || (connecting && !stalledConnect))) return false
      if (force && connecting && !stalledConnect) return false

      pendingReconnectRef.current = false
      connectAttemptStartedAtRef.current = now
      lastReconnectTriggerAtRef.current = now
      pushConnectTrace(`${source} -> reconnect`)
      setWsConnecting(true)
      setWsReconnectKey((k) => k + 1)
      return true
    },
    [pushConnectTrace]
  )

  const sendInput = useCallback((data: string) => {
    socketRef.current?.sendInput(data)
  }, [])

  const showKeyError = useCallback((message: string) => {
    termRef.current?.writeln(`\r\n\x1b[31mKey input error: ${message}\x1b[0m`)
  }, [])

  useEffect(() => {
    return () => {
      if (outputFlushRafRef.current !== null) {
        cancelAnimationFrame(outputFlushRafRef.current)
      }
      if (resizeDebounceRef.current !== null) {
        clearTimeout(resizeDebounceRef.current)
      }
    }
  }, [])

  useEffect(() => {
    replaySpeedRef.current = replaySpeed
  }, [replaySpeed])
  useEffect(() => {
    replayIdxRef.current = replayIdx
  }, [replayIdx])
  useEffect(() => {
    isPausedRef.current = isPaused
  }, [isPaused])
  useEffect(() => {
    isRunningRef.current = isSessionRunning(session)
  }, [session])
  useEffect(() => {
    wsConnectedRef.current = wsConnected
  }, [wsConnected])
  useEffect(() => {
    wsConnectingRef.current = wsConnecting
  }, [wsConnecting])
  useEffect(() => {
    modeRef.current = mode
  }, [mode])

  useEffect(() => {
    const handleOnline = () => setIsOnline(true)
    const handleOffline = () => setIsOnline(false)
    window.addEventListener('online', handleOnline)
    window.addEventListener('offline', handleOffline)
    return () => {
      window.removeEventListener('online', handleOnline)
      window.removeEventListener('offline', handleOffline)
    }
  }, [])

  const fetchMoreLogs = useCallback(async () => {
    if (!id || isFetchingMoreRef.current) return
    if (totalLinesRef.current > 0 && nextOffsetRef.current >= totalLinesRef.current) return
    isFetchingMoreRef.current = true
    try {
      const res = await fetchLogs(
        id,
        { offset: nextOffsetRef.current, limit: 500 },
        node ?? undefined
      )
      if (!isMounted.current) return
      if (res.lines.length > 0) {
        const next = [...logLinesRef.current, ...res.lines]
        logLinesRef.current = next
        setLogLines([...next])
        setScrubberMax(next.length)
        if (!isReplayingRef.current) {
          termRef.current?.write(res.lines.join(''))
          termRef.current?.scrollToBottom()
          setReplayIdx(next.length)
        }
      }
      if (res.total !== totalLinesRef.current) {
        totalLinesRef.current = res.total
        setTotalLines(res.total)
      }
      nextOffsetRef.current = res.next_offset
    } catch {
      /* ignore */
    } finally {
      isFetchingMoreRef.current = false
    }
  }, [id, node])

  const fetchMoreLogsRef = useRef<(() => Promise<void>) | null>(null)
  useEffect(() => {
    fetchMoreLogsRef.current = fetchMoreLogs
  }, [fetchMoreLogs])

  useEffect(() => {
    const el = termContainerRef.current
    if (!el || mode !== 'logs') return
    const handleWheel = (e: WheelEvent) => {
      if (
        e.deltaY > 0 &&
        !isFetchingMoreRef.current &&
        nextOffsetRef.current < totalLinesRef.current
      ) {
        fetchMoreLogsRef.current?.()
      } else if (e.deltaY < 0 && replayIdxRef.current > 0) {
        const step = e.shiftKey ? 50 : 10
        handleScrubRef.current?.(Math.max(0, replayIdxRef.current - step))
      }
    }
    el.addEventListener('wheel', handleWheel, { capture: true, passive: true })
    return () => el.removeEventListener('wheel', handleWheel, true)
  }, [mode])

  useEffect(() => {
    if (mode !== 'logs') return
    const handleKeyUp = (e: KeyboardEvent) => {
      if (
        (e.key === 'PageDown' || e.key === 'ArrowDown' || e.key === 'ArrowRight') &&
        !isFetchingMoreRef.current &&
        nextOffsetRef.current < totalLinesRef.current
      ) {
        e.preventDefault()
        fetchMoreLogsRef.current?.()
      } else if (e.key === 'PageUp' || e.key === 'ArrowUp' || e.key === 'ArrowLeft') {
        if (replayIdxRef.current > 0) {
          e.preventDefault()
          const step = e.key === 'PageUp' ? 50 : 10
          handleScrubRef.current?.(Math.max(0, replayIdxRef.current - step))
        }
      }
    }
    window.addEventListener('keyup', handleKeyUp)
    return () => window.removeEventListener('keyup', handleKeyUp)
  }, [mode])

  useEffect(() => {
    if (!id) return
    isMounted.current = true
    fetchSession(id, node ?? undefined)
      .then((s) => {
        if (isMounted.current) setSession(s)
      })
      .catch(() => {})
    return () => {
      isMounted.current = false
    }
  }, [id, node, reloadTick])

  useEffect(() => {
    if (mode !== 'attach' || !id) return

    // ── WebSocket attach (local + remote node) ─────────────────────────────
    pushConnectTrace('mode entered realtime connect view')
    // Mark the start of this connection attempt before any async work so that
    // the visibilitychange / pageshow handlers (which fire during iOS PWA app
    // launch / page transitions) see a fresh attempt and do NOT trigger a
    // spurious reconnect.  Without this, connectAttemptStartedAtRef is 0
    // (initial) which makes stalledConnect=true, bypassing the
    // "don't reconnect while already connecting" guard in requestReconnect.
    connectAttemptStartedAtRef.current = Date.now()
    lastReconnectTriggerAtRef.current = Date.now()
    setWsConnecting(true)

    // ended: server sent an 'end' frame — session finished normally, no reconnect.
    // discarded: this effect run is being cleaned up — prevents stale onClose from
    // scheduling a reconnect after we've already torn down intentionally.
    let ended = false
    let discarded = false
    let gotSnapshot = false

    const scheduleReconnect = (code: number, reason: string) => {
      setWsConnecting(true)
      const attempt = reconnectAttemptRef.current + 1
      reconnectAttemptRef.current = attempt
      const delay = Math.min(2000, 120 * 2 ** Math.min(6, attempt - 1))
      const hidden = document.visibilityState !== 'visible'
      const offline = typeof navigator !== 'undefined' && !navigator.onLine

      if (reconnectTimerRef.current !== null) {
        clearTimeout(reconnectTimerRef.current)
        reconnectTimerRef.current = null
      }

      if (hidden || offline) {
        pendingReconnectRef.current = true
        pushConnectTrace(
          `reconnect deferred (${hidden ? 'hidden' : ''}${hidden && offline ? '+' : ''}${offline ? 'offline' : ''}) attempt=${attempt}`
        )
        setWsError(null)
        return
      }

      pendingReconnectRef.current = false
      const transientClose = code === 1006 || code === 1001 || code === 1005 || code === 0
      if (!transientClose) {
        pushConnectTrace(
          `non-transient close treated as retryable (code=${code}${reason ? ` reason=${reason}` : ''}) attempt=${attempt}`
        )
      }
      setWsError(null)

      reconnectTimerRef.current = window.setTimeout(() => {
        if (!discarded && isMounted.current && modeRef.current === 'attach') {
          pushConnectTrace(`retry timer fired (attempt=${attempt}) -> reconnect`)
          setWsReconnectKey((k) => k + 1)
        }
      }, delay)
    }

    connectAttemptStartedAtRef.current = Date.now()
    // Force a synchronous fit so we send the real terminal dimensions (not the
    // default 80×24 before FitAddon's deferred RAF has fired).
    const initialSize = termRef.current?.fit() ?? undefined
    const sock = new AttachSocket(
      id,
      {
        onOpen: () => {
          pushConnectTrace('websocket open')
          reconnectAttemptRef.current = 0
          pendingReconnectRef.current = false
          connectAttemptStartedAtRef.current = 0
          lastSentResizeRef.current = null
          pendingResizeRef.current = null
          lastWsFrameAtRef.current = Date.now()
          setWsError(null)
          setWsConnecting(false)
          setWsEverConnected(true)
          if (reconnectTimerRef.current !== null) {
            clearTimeout(reconnectTimerRef.current)
            reconnectTimerRef.current = null
          }
          if (isMounted.current) setWsConnected(true)
        },
        onInit: (data, _appCursorKeys, _bracketedPasteMode) => {
          if (!gotSnapshot) {
            pushConnectTrace(`init received (${data.length} bytes)`)
            gotSnapshot = true
          }
          lastWsFrameAtRef.current = Date.now()
          enqueueTerminalOutput([data], { reset: true })
        },
        onData: (data) => {
          lastWsFrameAtRef.current = Date.now()
          enqueueTerminalOutput([data])
        },
        onModeChanged: (_appCursorKeys, _bracketedPasteMode) => {
          lastWsFrameAtRef.current = Date.now()
          // Mode changes are tracked server-side; client doesn't need to act.
        },
        onSessionEnded: (code) => {
          ended = true
          lastWsFrameAtRef.current = Date.now()
          pushConnectTrace(`server end frame received (exit=${code ?? 'null'})`)
          if (!isMounted.current) return
          const exitMsg = code != null ? ` (exit code: ${code})` : ''
          termRef.current?.writeln(`\r\n\x1b[2m[Session ended${exitMsg}]\x1b[0m`)
          setExitCode(code)
          setWsConnected(false)
          setSearchParams(node ? { mode: 'logs', node } : { mode: 'logs' })
          fetchSession(id!, node ?? undefined)
            .then((s) => {
              if (isMounted.current) setSession(s)
            })
            .catch(() => {})
        },
        onError: (msg) => {
          lastWsFrameAtRef.current = Date.now()
          pushConnectTrace(`server error frame: ${msg}`)
          if (!isMounted.current) return
          termRef.current?.writeln(`\r\n\x1b[31mError: ${msg}\x1b[0m`)
          setWsError(`Server error: ${msg}`)
        },
        onClose: (code, reason) => {
          pushConnectTrace(`websocket close (code=${code}${reason ? ` reason=${reason}` : ''})`)
          if (isMounted.current) setWsConnected(false)
          // iOS PWA kills WebSocket connections when the app goes to background.
          // Reconnect automatically for unexpected drops (not when the session
          // ended normally or this effect is being cleaned up).
          if (!ended && !discarded) {
            setWsConnecting(true)
            scheduleReconnect(code, reason)
          }
        },
      },
      node ?? undefined,
      initialSize ?? undefined
    )
    pushConnectTrace('websocket created')
    socketRef.current = sock

    return () => {
      discarded = true
      if (reconnectTimerRef.current !== null) {
        clearTimeout(reconnectTimerRef.current)
        reconnectTimerRef.current = null
      }
      if (outputFlushRafRef.current !== null) {
        cancelAnimationFrame(outputFlushRafRef.current)
        outputFlushRafRef.current = null
      }
      outputBufferRef.current = []
      pendingResetRef.current = false
      pushConnectTrace('teardown current websocket')
      sock.close()
      socketRef.current = null
      setWsConnected(false)
      setWsConnecting(false)
    }
  }, [mode, id, node, pushConnectTrace, setSearchParams, wsReconnectKey, enqueueTerminalOutput])

  useEffect(() => {
    if (mode !== 'attach') setWsConnecting(false)
  }, [mode])

  // iOS PWA: reconnect the WebSocket immediately when the app returns from
  // background. iOS can resume with a stale "connected" socket state before
  // onclose arrives, so force a reconnect on foreground transitions.
  useEffect(() => {
    if (mode !== 'attach') return
    const triggerReconnect = (source: string) => {
      const now = Date.now()
      const recentlyReceivedFrame = now - lastWsFrameAtRef.current < 5000
      const forceReconnect = !wsConnectedRef.current || !recentlyReceivedFrame
      requestReconnect(source, forceReconnect)
    }

    const handleVisibilityChange = () => {
      if (document.visibilityState === 'visible') {
        triggerReconnect('visibilitychange -> visible')
      }
    }
    const handlePageShow = (event: PageTransitionEvent) => {
      if (document.visibilityState === 'visible') {
        triggerReconnect(`pageshow(persisted=${event.persisted})`)
      }
    }
    const handleOnline = () => {
      if (pendingReconnectRef.current || !wsConnectedRef.current) {
        requestReconnect('online', true)
      }
    }
    document.addEventListener('visibilitychange', handleVisibilityChange)
    window.addEventListener('pageshow', handlePageShow)
    window.addEventListener('online', handleOnline)
    return () => {
      document.removeEventListener('visibilitychange', handleVisibilityChange)
      window.removeEventListener('pageshow', handlePageShow)
      window.removeEventListener('online', handleOnline)
    }
  }, [mode, requestReconnect])

  // Reconnect watchdog: if attach view is visible+online but remains disconnected
  // without a scheduled retry, force a fresh socket attempt. This covers stale
  // iOS PWA states where close events are delayed or dropped.
  useEffect(() => {
    if (mode !== 'attach') return
    const tick = window.setInterval(() => {
      if (document.visibilityState !== 'visible') return
      if (typeof navigator !== 'undefined' && !navigator.onLine) return
      if (wsConnectedRef.current) return
      if (reconnectTimerRef.current !== null) return

      const startedAt = connectAttemptStartedAtRef.current
      const elapsed = startedAt > 0 ? Date.now() - startedAt : Number.POSITIVE_INFINITY
      const currentlyConnecting = wsConnectingRef.current
      const minimumElapsed = currentlyConnecting ? 8000 : 2500
      if (elapsed < minimumElapsed) return

      requestReconnect(`watchdog${currentlyConnecting ? ' (stalled connect attempt)' : ''}`, true)
    }, 1600)

    return () => window.clearInterval(tick)
  }, [mode, requestReconnect])

  useEffect(() => {
    if (mode !== 'logs' || !id) return
    termRef.current?.reset()
    setReplayIdx(0)
    isReplayingRef.current = false
    setIsReplaying(false)
    setIsPaused(false)
    isPausedRef.current = false
    logLinesRef.current = []
    setLogLines([])
    setScrubberMax(0)
    totalLinesRef.current = 0
    nextOffsetRef.current = 0
    isFetchingMoreRef.current = false
    setTotalLines(0)

    let cancelled = false
    fetchLogs(id!, { offset: 0, limit: 200 }, node ?? undefined)
      .then((res) => {
        if (cancelled || !isMounted.current) return
        logLinesRef.current = res.lines
        setLogLines(res.lines)
        totalLinesRef.current = res.total
        setTotalLines(res.total)
        nextOffsetRef.current = res.next_offset
        setScrubberMax(res.lines.length)
        if (res.lines.length > 0) {
          termRef.current?.write(res.lines.join(''))
          termRef.current?.scrollToBottom()
        }
        setReplayIdx(res.lines.length)
        fetchSession(id!, node ?? undefined)
          .then((s) => {
            if (isMounted.current) setSession(s)
          })
          .catch(() => {})
      })
      .catch(() => {})

    return () => {
      cancelled = true
    }
  }, [mode, id, node, reloadTick])

  const handleScrubRef = useRef<((val: number) => void) | null>(null)
  function handleScrub(val: number) {
    if (replayRafRef.current !== null) {
      cancelAnimationFrame(replayRafRef.current)
      replayRafRef.current = null
      isReplayingRef.current = false
      setIsReplaying(false)
      setIsPaused(false)
      isPausedRef.current = false
    }
    setReplayIdx(val)
    termRef.current?.reset()
    if (logLinesRef.current.length > 0) {
      termRef.current?.write(logLinesRef.current.slice(0, val + 1).join(''))
    }
    termRef.current?.scrollToBottom()
    if (
      val >= logLinesRef.current.length - 1 &&
      !isFetchingMoreRef.current &&
      nextOffsetRef.current < totalLinesRef.current
    ) {
      fetchMoreLogsRef.current?.()
    }
  }
  handleScrubRef.current = handleScrub

  function startReplay(fromIdx = 0) {
    if (replayRafRef.current !== null) {
      cancelAnimationFrame(replayRafRef.current)
      replayRafRef.current = null
    }
    setIsPaused(false)
    isPausedRef.current = false
    isReplayingRef.current = true
    setIsReplaying(true)
    let idx = fromIdx
    if (fromIdx === 0) {
      setReplayIdx(0)
      termRef.current?.reset()
    }
    function step() {
      if (isPausedRef.current) {
        replayRafRef.current = null
        return
      }
      const lines = logLinesRef.current
      if (idx >= lines.length) {
        if (nextOffsetRef.current < totalLinesRef.current) {
          if (!isFetchingMoreRef.current) fetchMoreLogsRef.current?.()
          replayRafRef.current = requestAnimationFrame(step)
          return
        }
        replayRafRef.current = null
        isReplayingRef.current = false
        setIsReplaying(false)
        setIsPaused(false)
        isPausedRef.current = false
        return
      }
      const BATCH = Math.max(1, Math.round(5 * replaySpeedRef.current))
      termRef.current?.write(lines.slice(idx, idx + BATCH).join(''))
      idx += BATCH
      setReplayIdx(idx)
      replayRafRef.current = requestAnimationFrame(step)
    }
    replayRafRef.current = requestAnimationFrame(step)
  }

  async function handleLoadPageAndReplay() {
    if (nextOffsetRef.current < totalLinesRef.current) {
      await fetchMoreLogsRef.current?.()
    }
  }

  function handleReplayButton() {
    if (!isReplaying) {
      startReplay(0)
    } else if (!isPaused) {
      setIsPaused(true)
      isPausedRef.current = true
      if (replayRafRef.current !== null) {
        cancelAnimationFrame(replayRafRef.current)
        replayRafRef.current = null
      }
    } else {
      setIsPaused(false)
      isPausedRef.current = false
      startReplay(replayIdx)
    }
  }

  async function handleStop() {
    if (!id) return
    await stopSession(id, undefined, node ?? undefined).catch(() => {})
    fetchSession(id, node ?? undefined)
      .then((s) => {
        if (isMounted.current) setSession(s)
      })
      .catch(() => {})
  }
  async function handleKill() {
    if (!id) return
    await killSession(id, node ?? undefined).catch(() => {})
    fetchSession(id, node ?? undefined)
      .then((s) => {
        if (isMounted.current) setSession(s)
      })
      .catch(() => {})
  }

  function handleTermResize(cols: number, rows: number) {
    const pending = pendingResizeRef.current
    if (pending && pending.cols === cols && pending.rows === rows) return
    pendingResizeRef.current = { cols, rows }

    if (resizeDebounceRef.current !== null) {
      clearTimeout(resizeDebounceRef.current)
    }
    resizeDebounceRef.current = window.setTimeout(() => {
      resizeDebounceRef.current = null
      const next = pendingResizeRef.current
      if (!next) return
      const last = lastSentResizeRef.current
      if (last && last.cols === next.cols && last.rows === next.rows) return
      socketRef.current?.sendResize(next.rows, next.cols)
      if (wsConnectedRef.current) {
        lastSentResizeRef.current = next
      }
    }, 120)
  }

  function handleManualRefresh() {
    setReloadTick((tick) => tick + 1)
    if (mode === 'attach') {
      requestReconnect('manual refresh', true)
    }
  }

  const isRunning = isSessionRunning(session)

  // Unused var suppression
  void logLines

  const attachedState = (
    <div className="flex items-center gap-2 opacity-60 text-xs">
      {mode === 'attach' &&
        (wsConnected ? (
          <>
            <Link1Icon className="h-4 w-4" />
            <span>Attached</span>
          </>
        ) : wsConnecting ? (
          <>
            <TrackNextIcon className="h-4 w-4" />
            {wsEverConnected ? 'Reconnecting' : 'Connecting'}
          </>
        ) : (
          <>
            <CrossCircledIcon className="h-4 w-4" />
            Disconnected
          </>
        ))}
    </div>
  )

  return (
    <TooltipProvider>
      <div className="flex flex-col bg-[hsl(var(--background))] text-[hsl(var(--foreground))] h-full">
        {/* ── Header ── */}
        <header className="flex flex-wrap items-center gap-x-3 gap-y-1.5 px-3 sm:px-4 py-2 sm:py-2.5 border-b border-[hsl(var(--border))] bg-[hsl(var(--background))]/95 sticky top-0 z-30 backdrop-blur shrink-0">
          <Link to="/">
            <div className="flex items-center gap-2 text-[hsl(var(--primary))] font-bold text-lg select-none">
              <Logo />
              <span className="hidden sm:inline">Open Relay</span>
            </div>
          </Link>
          <div className="flex items-center gap-3 min-w-0 flex-1">
            <span className="font-mono text-sm text-[hsl(var(--muted-foreground))] font-semibold truncate">
              {session?.id}
            </span>
            {session && <StatusBadge status={session.status} inputNeeded={session.input_needed} />}
            {node && (
              <Badge className="inline-flex font-light border-[hsl(var(--primary))]/40 bg-[hsl(var(--primary))]/10 text-[hsl(var(--primary))] text-xs">
                {node}
              </Badge>
            )}
            <div className="hidden sm:inline-block">{attachedState}</div>
            {mode === 'attach' && !isOnline && (
              <Badge className="inline-flex font-light border-amber-400/40 bg-amber-400/10 text-amber-600 dark:text-amber-300">
                <TrackNextIcon className="h-4 w-4" />
                <span className="hidden sm:inline">Offline</span>
              </Badge>
            )}
          </div>

          {/* Desktop actions */}
          <div className="hidden sm:flex items-center gap-2 flex-wrap justify-end">
            <Button size="sm" variant="ghost" onClick={handleManualRefresh}>
              <ReloadIcon className="h-4 w-4" />
              Refresh
            </Button>
            {mode === 'attach' && (
              <Button
                size="sm"
                variant="ghost"
                onClick={() => setSearchParams(node ? { mode: 'logs', node } : { mode: 'logs' })}
              >
                <FileTextIcon className="h-4 w-4" />
                View Logs
              </Button>
            )}
            {/* {mode === 'attach' && (
              <Button size="sm" variant="ghost" onClick={() => setConnectTraceOpen(true)}>
                <TrackNextIcon className="h-4 w-4" />
                Trace
              </Button>
            )} */}
            {isRunning && (
              <>
                <Button size="sm" variant="stop" onClick={() => setPendingAction('stop')}>
                  <StopIcon className="h-4 w-4" />
                  Stop
                </Button>
                <Button size="sm" variant="kill" onClick={() => setPendingAction('kill')}>
                  <Cross2Icon className="h-4 w-4" />
                  Kill
                </Button>
              </>
            )}
            {mode === 'logs' && isRunning && (
              <Button
                size="sm"
                onClick={() =>
                  setSearchParams(node ? { mode: 'attach', node } : { mode: 'attach' })
                }
              >
                <LinkNone2Icon className="h-4 w-4" />
                Attach
              </Button>
            )}
          </div>

          {/* Mobile actions via dropdown */}
          <div className="sm:hidden shrink-0">
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <Button variant="ghost" size="icon" aria-label="Session actions">
                  <DotsVerticalIcon className="h-4 w-4" />
                </Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end">
                <DropdownMenuItem onClick={handleManualRefresh}>
                  <ReloadIcon className="w-4 h-4" />
                  Refresh
                </DropdownMenuItem>
                {(mode === 'attach' || isRunning) && (
                  <>
                    {mode === 'attach' && (
                      <DropdownMenuItem
                        onClick={() =>
                          setSearchParams(node ? { mode: 'logs', node } : { mode: 'logs' })
                        }
                      >
                        <FileTextIcon className="w-4 h-4" />
                        View Logs
                      </DropdownMenuItem>
                    )}
                    {mode === 'attach' && (
                      <DropdownMenuItem onClick={() => setConnectTraceOpen(true)}>
                        <TrackNextIcon className="w-4 h-4" />
                        Trace
                      </DropdownMenuItem>
                    )}
                    {isRunning && (
                      <DropdownMenuItem
                        className="text-amber-400 focus:text-amber-300"
                        onClick={() => setPendingAction('stop')}
                      >
                        <StopIcon className="w-4 h-4" />
                        Stop
                      </DropdownMenuItem>
                    )}
                    {isRunning && (
                      <DropdownMenuItem
                        className="text-red-400 focus:text-red-300"
                        onClick={() => setPendingAction('kill')}
                      >
                        <Cross2Icon className="w-4 h-4" />
                        Kill
                      </DropdownMenuItem>
                    )}
                    {mode === 'logs' && isRunning && (
                      <DropdownMenuItem
                        className="text-indigo-400 focus:text-indigo-300"
                        onClick={() =>
                          setSearchParams(node ? { mode: 'attach', node } : { mode: 'attach' })
                        }
                      >
                        <LinkNone2Icon className="w-4 h-4" />
                        Attach
                      </DropdownMenuItem>
                    )}
                  </>
                )}
              </DropdownMenuContent>
            </DropdownMenu>
          </div>
        </header>

        {/* ── Info bar ── */}
        {session && (
          <div className="flex items-center gap-x-4 gap-y-0.5 px-3 sm:px-4 py-1.5 border-b border-[hsl(var(--border))] bg-[hsl(var(--card))]/60 text-xs text-[hsl(var(--muted-foreground))] shrink-0 flex-wrap">
            <span className="text-[hsl(var(--foreground))] font-mono truncate max-w-[50vw] sm:max-w-none">
              {session.command} {session.args.join(' ')}
            </span>
            {session.cwd && (
              <span className="hidden sm:inline">
                CWD: <span className="text-[hsl(var(--foreground))] font-mono">{session.cwd}</span>
              </span>
            )}
            {session?.title && (
              <span className="hidden sm:inline text-sm font-medium text-[hsl(var(--foreground))] truncate max-w-40 sm:max-w-xs">
                {session.title}
              </span>
            )}
            <span>
              Started:{' '}
              <span className="text-[hsl(var(--foreground))]">{formatAge(session.created_at)}</span>
            </span>
            {exitCode !== undefined && (
              <span>
                Exit: <span className="text-[hsl(var(--foreground))]">{exitCode ?? '?'}</span>
              </span>
            )}
            {session.pid != null && (
              <span className="hidden sm:inline">
                PID: <span className="text-[hsl(var(--foreground))]">{session.pid}</span>
              </span>
            )}
          </div>
        )}

        {/* ── Main body ── */}
        <div
          id="main-container"
          className="sm:flex overflow-y-auto sm:overflow-hidden flex-1 min-h-0"
        >
          {wsError && <div className="text-red-500 text-sm">{wsError}</div>}

          {/* Terminal area */}
          <div
            className={`flex flex-col flex-1 w-full overflow-hidden ${mode === 'logs' ? 'h-full' : 'h-[calc(100%-72px)] sm:h-full'}`}
          >
            <div
              ref={termContainerRef}
              className={`relative flex-1 min-h-0 bg-[hsl(var(--terminal-bg))] py-2 px-4`}
            >
              <div className="h-full w-full overflow-x-auto">
                <div className={`h-full ${mode === 'logs' ? 'w-500 min-w-full' : ''}`}>
                  <XTerm
                    ref={termRef}
                    onData={mode === 'attach' ? sendInput : undefined}
                    onResize={mode === 'attach' ? handleTermResize : undefined}
                    className="h-full"
                  />
                </div>
              </div>
              <div
                className="absolute sm:hidden right-0 top-0 bottom-0 w-10 bg-transparent"
                onPointerMove={(e) => {
                  if (mode === 'logs') {
                    handleScrubRef.current?.(Math.max(0, replayIdxRef.current + e.movementY * 2))
                  } else {
                    termRef?.current?.scrollLines(e.movementY)
                  }
                }}
              ></div>
            </div>

            {/* Scrubber (logs mode) */}
            {mode === 'logs' && scrubberMax > 0 && (
              <div className="flex flex-row gap-2 px-3 sm:px-4 py-2 border-t border-[hsl(var(--border))] bg-[hsl(var(--card))]/80 shrink-0">
                <div className="flex flex-1 items-center gap-2">
                  <Slider
                    className="flex-1"
                    min={0}
                    max={scrubberMax}
                    value={[replayIdx]}
                    onValueChange={(value) => handleScrub(value[0] ?? 0)}
                    aria-label="Replay scrubber"
                  />
                  <span className="hidden sm:inline text-sm text-[hsl(var(--muted-foreground))] tabular-nums whitespace-nowrap">
                    {totalLines > scrubberMax ? `${scrubberMax}/${totalLines}` : scrubberMax}
                  </span>
                </div>
                <div className="flex items-center gap-1.5">
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <Button
                        variant="secondary"
                        size="icon"
                        onClick={() =>
                          handleScrubRef.current?.(Math.max(0, replayIdxRef.current - 10))
                        }
                      >
                        <ChevronLeftIcon className="h-4 w-4" />
                      </Button>
                    </TooltipTrigger>
                    <TooltipContent>Back 10 lines</TooltipContent>
                  </Tooltip>
                  {totalLines > scrubberMax && (
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button variant="secondary" size="icon" onClick={handleLoadPageAndReplay}>
                          <ChevronRightIcon className="h-4 w-4" />
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent>Load next page</TooltipContent>
                    </Tooltip>
                  )}
                  <div className="flex items-center gap-1 ml-auto">
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button variant="secondary" size="icon" onClick={handleReplayButton}>
                          {!isReplaying ? (
                            <PlayIcon className="h-4 w-4" />
                          ) : isPaused ? (
                            <PlayIcon className="h-4 w-4" />
                          ) : (
                            <PauseIcon className="h-4 w-4" />
                          )}
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent>
                        {!isReplaying ? 'Replay' : isPaused ? 'Resume' : 'Pause'}
                      </TooltipContent>
                    </Tooltip>
                    <Select
                      value={String(replaySpeed)}
                      onValueChange={(v) => setReplaySpeed(Number(v))}
                    >
                      <SelectTrigger className="h-8 w-15 text-sm px-2">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="0.5">0.5×</SelectItem>
                        <SelectItem value="1">1×</SelectItem>
                        <SelectItem value="2">2×</SelectItem>
                        <SelectItem value="5">5×</SelectItem>
                        <SelectItem value="10">10×</SelectItem>
                      </SelectContent>
                    </Select>
                  </div>
                </div>
              </div>
            )}
          </div>

          {mode === 'attach' && (
            <>
              <div className="overflow-hidden rounded-t-md bg-[hsl(var(--card))]/90 sm:rounded-none">
                <AttachPanel sendInput={sendInput} showKeyError={showKeyError} />
              </div>
              <div className="sm:hidden flex items-center justify-center h-8">{attachedState}</div>
            </>
          )}
        </div>

        {/* Navigate back button (hide in logs mode to avoid overlap with scrubber controls) */}
        {mode !== 'attach' && mode !== 'logs' && (
          <Button
            variant="outline"
            size="icon"
            className="sm:hidden fixed bottom-4 left-4 z-50 h-10 w-10 rounded-full shadow-lg"
            onClick={() => navigate('/')}
            aria-label="Back to sessions"
          >
            ←
          </Button>
        )}

        <ConfirmActionDialog
          action={pendingAction}
          sessionId={id ?? ''}
          onConfirm={(action) => {
            if (action === 'stop') void handleStop()
            else void handleKill()
          }}
          onClose={() => setPendingAction(null)}
        />
        {/* 
        <Dialog
          open={wsError !== null}
          onOpenChange={(open) => {
            if (!open) setWsError(null)
          }}
        >
          <DialogContent className="max-w-lg">
            <DialogHeader>
              <DialogTitle className="text-red-400">Attach Error</DialogTitle>
            </DialogHeader>
            <p className="text-xs text-[hsl(var(--muted-foreground))] mb-2">
              Raw error detail for debugging:
            </p>
            <pre className="text-xs text-[hsl(var(--foreground))] bg-[hsl(var(--secondary))] rounded p-3 overflow-x-auto whitespace-pre-wrap break-all border border-[hsl(var(--border))]">
              {wsError}
            </pre>
            <div className="flex justify-end gap-2 pt-1">
              <Button
                variant="ghost"
                size="sm"
                onClick={() => {
                  if (wsError) navigator.clipboard?.writeText(wsError).catch(() => { })
                }}
              >
                Copy
              </Button>
              <Button size="sm" onClick={() => setWsError(null)}>
                Dismiss
              </Button>
            </div>
          </DialogContent>
        </Dialog> */}

        <Dialog open={connectTraceOpen} onOpenChange={setConnectTraceOpen}>
          <DialogContent className="max-w-2xl">
            <DialogHeader>
              <DialogTitle>Realtime Connect Trace</DialogTitle>
            </DialogHeader>
            <p className="text-xs text-[hsl(var(--muted-foreground))] mb-2">
              Step-by-step lifecycle entries for diagnosing iOS standalone behavior.
            </p>
            <pre className="text-xs text-[hsl(var(--foreground))] bg-[hsl(var(--secondary))] rounded p-3 overflow-x-auto whitespace-pre-wrap break-all border border-[hsl(var(--border))] max-h-[55vh]">
              {connectTrace.length > 0 ? connectTrace.join('\n') : 'No trace entries yet.'}
            </pre>
            <div className="flex justify-end gap-2 pt-1">
              <Button variant="ghost" size="sm" onClick={() => setConnectTrace([])}>
                Clear
              </Button>
              <Button variant="ghost" size="sm" onClick={() => setConnectTraceOpen(false)}>
                Close
              </Button>
            </div>
          </DialogContent>
        </Dialog>
      </div>
    </TooltipProvider>
  )
}
