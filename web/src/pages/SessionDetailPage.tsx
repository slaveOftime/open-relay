import { useState, useEffect, useRef, useCallback } from 'react'
import { useParams, useSearchParams, useNavigate } from 'react-router-dom'
import type { SessionSummary } from '@/api/types'
import {
  fetchSession,
  fetchLogs,
  fetchLogsTail,
  stopSession,
  killSession,
  uploadSessionFile,
  AttachSocket,
} from '@/api/client'
import { formatByteSize, formatTimestamp, sessionDisplayName } from '@/utils/format'
import {
  encodeLogChunks,
  initialLogReplayState,
  playNextBatch,
  replayLogChunks,
  seekLogChunks,
  type LogReplayState,
} from '@/utils/logReplay'
import StatusBadge from '@/components/StatusBadge'
import CommandLogo from '@/components/CommandLogo'
import XTerm, { type XTermHandle } from '@/components/XTerm'
import Logo from '@/components/Logo'
import NewSessionDialog, { buildNewSessionInitialValues } from '@/components/NewSessionDialog'
import SessionMetadataDialog from '@/components/SessionMetadataDialog'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { getTransferredFiles } from '@/components/ui/file-transfer'
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from '@/components/ui/dropdown-menu'
// import {
//   Select,
//   SelectContent,
//   SelectItem,
//   SelectTrigger,
//   SelectValue,
// } from '@/components/ui/select'
import { Slider } from '@/components/ui/slider'
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip'
import {
  ChevronLeftIcon,
  ChevronRightIcon,
  CopyIcon,
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

function normalizeSnapshotOutputForXterm(output: Uint8Array): Uint8Array {
  let extra = 0
  for (let i = 0; i < output.length; i += 1) {
    if (output[i] === 0x0a && (i === 0 || output[i - 1] !== 0x0d)) {
      extra += 1
    }
  }
  if (extra === 0) return output

  const normalized = new Uint8Array(output.length + extra)
  let writeIndex = 0
  for (let i = 0; i < output.length; i += 1) {
    const byte = output[i]
    if (byte === 0x0a && (i === 0 || output[i - 1] !== 0x0d)) {
      normalized[writeIndex] = 0x0d
      writeIndex += 1
    }
    normalized[writeIndex] = byte
    writeIndex += 1
  }
  return normalized
}

const DEFAULT_LOG_TAIL = 200
const ATTACH_IDLE_BORDER_DELAY_MS = 10_000

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
  const [searchParams] = useSearchParams()
  const reloadKey = searchParams.toString()

  return <SessionDetailPageContent key={reloadKey} />
}

function SessionDetailPageContent() {
  const { id } = useParams<{ id: string }>()
  const [searchParams, setSearchParams] = useSearchParams()
  const navigate = useNavigate()

  const mode = (searchParams.get('mode') ?? 'logs') as 'attach' | 'logs'
  const logsView = searchParams.get('view') === 'replay' ? 'replay' : 'tail'
  const isTailMode = logsView === 'tail'
  const node = searchParams.get('node')

  const [session, setSession] = useState<SessionSummary | null>(null)
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
  const [replaySpeed] = useState(0.5)
  const [totalChunks, setTotalChunks] = useState(0)
  const [reloadTick, setReloadTick] = useState(0)
  const [isOnline, setIsOnline] = useState(
    typeof navigator === 'undefined' ? true : navigator.onLine
  )
  const [isInfoBarToggled, setIsInfoBarToggled] = useState(false)
  const [tailLimit, setTailLimit] = useState<number | null>(null)
  const [tailLimitInput, setTailLimitInput] = useState('40')
  const [isAttachViewportIdle, setIsAttachViewportIdle] = useState(false)
  const [showNewSessionDialog, setShowNewSessionDialog] = useState(false)
  const [showMetadataDialog, setShowMetadataDialog] = useState(false)

  const termRef = useRef<XTermHandle>(null)
  const socketRef = useRef<AttachSocket | null>(null)
  const wsConnectedRef = useRef(false)
  const wsConnectingRef = useRef(false)
  const modeRef = useRef(mode)
  const replayTimerRef = useRef<number | null>(null)
  const logChunksRef = useRef<Uint8Array[]>([])
  const replaySpeedRef = useRef(0.5)
  const isPausedRef = useRef(false)
  const isRunningRef = useRef(false)
  const isReplayingRef = useRef(false)
  const totalChunksRef = useRef(0)
  const logReplayStateRef = useRef<LogReplayState>(initialLogReplayState())
  const logResizesRef = useRef<{ offset: number; rows: number; cols: number }[]>([])
  const loadedLogCountRef = useRef(0)
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
  const outputWriteInFlightRef = useRef(false)
  const pendingResetRef = useRef(false)
  const resizeDebounceRef = useRef<number | null>(null)
  const pendingResizeRef = useRef<{ cols: number; rows: number } | null>(null)
  const lastSentResizeRef = useRef<{ cols: number; rows: number } | null>(null)
  const lastReconnectTriggerAtRef = useRef(0)
  const lastWsFrameAtRef = useRef(0)
  const replayUiLastCommitAtRef = useRef(0)
  const replayCommittedIdxRef = useRef(0)
  const attachIdleTimerRef = useRef<number | null>(null)
  const attachIdleCountdownArmedRef = useRef(false)

  const clearAttachIdleTimer = useCallback(() => {
    if (attachIdleTimerRef.current !== null) {
      clearTimeout(attachIdleTimerRef.current)
      attachIdleTimerRef.current = null
    }
  }, [])

  const stopAttachIdleAnimation = useCallback(() => {
    clearAttachIdleTimer()
    if (isMounted.current) {
      setIsAttachViewportIdle(false)
    }
  }, [clearAttachIdleTimer])

  const disarmAttachIdleAnimation = useCallback(() => {
    attachIdleCountdownArmedRef.current = false
    stopAttachIdleAnimation()
  }, [stopAttachIdleAnimation])

  const scheduleAttachIdleAnimation = useCallback(() => {
    clearAttachIdleTimer()
    if (
      !attachIdleCountdownArmedRef.current ||
      modeRef.current !== 'attach' ||
      !wsConnectedRef.current
    ) {
      if (isMounted.current) {
        setIsAttachViewportIdle(false)
      }
      return
    }

    attachIdleTimerRef.current = window.setTimeout(() => {
      attachIdleTimerRef.current = null
      if (
        !isMounted.current ||
        modeRef.current !== 'attach' ||
        !wsConnectedRef.current ||
        !attachIdleCountdownArmedRef.current
      ) {
        return
      }
      setSession((session) => ({ ...session, input_needed: true }) as SessionSummary)
      setIsAttachViewportIdle(true)
    }, ATTACH_IDLE_BORDER_DELAY_MS)
  }, [clearAttachIdleTimer])

  const noteAttachInboundData = useCallback(() => {
    if (!isMounted.current) return

    attachIdleCountdownArmedRef.current = true
    setIsAttachViewportIdle(false)
    scheduleAttachIdleAnimation()
  }, [scheduleAttachIdleAnimation])

  const noteAttachUserActivity = useCallback(() => {
    disarmAttachIdleAnimation()
  }, [disarmAttachIdleAnimation])

  const flushTerminalOutput = useCallback(() => {
    if (outputWriteInFlightRef.current) {
      return
    }
    const term = termRef.current
    if (!term) {
      outputBufferRef.current = []
      pendingResetRef.current = false
      outputWriteInFlightRef.current = false
      return
    }
    if (pendingResetRef.current) {
      term.reset()
      pendingResetRef.current = false
    }
    if (outputBufferRef.current.length === 0) {
      return
    }
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
    outputWriteInFlightRef.current = true
    term.write(merged, () => {
      outputWriteInFlightRef.current = false
      term.scrollToBottom()
      if (
        (pendingResetRef.current || outputBufferRef.current.length > 0) &&
        outputFlushRafRef.current === null
      ) {
        outputFlushRafRef.current = requestAnimationFrame(() => {
          outputFlushRafRef.current = null
          flushTerminalOutput()
        })
      }
    })
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
      if (outputFlushRafRef.current === null && !outputWriteInFlightRef.current) {
        outputFlushRafRef.current = requestAnimationFrame(() => {
          outputFlushRafRef.current = null
          flushTerminalOutput()
        })
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

  const sendInput = useCallback((data: string, waitForChange: boolean) => {
    socketRef.current?.sendInput(data, waitForChange)
  }, [])

  const sendBusy = useCallback(() => {
    socketRef.current?.sendBusy()
  }, [])

  const showKeyError = useCallback((message: string) => {
    termRef.current?.writeln(`\r\n\x1b[31mKey input error: ${message}\x1b[0m`)
  }, [])

  const handleUploadFile = useCallback(
    async (file: File) => {
      if (!id) {
        throw new Error('Session id is missing')
      }
      return await uploadSessionFile(id, file, node ?? undefined)
    },
    [id, node]
  )

  const handleTerminalPaste = useCallback(
    async (event: ClipboardEvent) => {
      if (mode !== 'attach') return

      const clipboardData = event.clipboardData
      if (!clipboardData) return

      const files = getTransferredFiles(clipboardData)
      if (files.length > 0) {
        event.preventDefault()
        try {
          const uploadedPaths: string[] = []
          for (const file of files) {
            const response = await handleUploadFile(file)
            if (response.ok) {
              uploadedPaths.push(response.path)
            }
          }
          if (uploadedPaths.length > 0) {
            sendInput(uploadedPaths.join(' '), false)
          }
        } catch (error) {
          showKeyError(error instanceof Error ? error.message : 'file upload failed')
        }
        return
      }

      const text = clipboardData.getData('text/plain')
      if (!text) return

      event.preventDefault()
      sendInput(text, false)
    },
    [handleUploadFile, mode, sendInput, showKeyError]
  )

  useEffect(() => {
    return () => {
      if (outputFlushRafRef.current !== null) {
        cancelAnimationFrame(outputFlushRafRef.current)
      }
      if (resizeDebounceRef.current !== null) {
        clearTimeout(resizeDebounceRef.current)
      }
      clearAttachIdleTimer()
    }
  }, [clearAttachIdleTimer])

  useEffect(() => {
    replaySpeedRef.current = replaySpeed
  }, [replaySpeed])
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
    if (mode !== 'attach' || !wsConnected) {
      disarmAttachIdleAnimation()
    }
  }, [disarmAttachIdleAnimation, mode, wsConnected])

  const commitReplayIdx = useCallback((idx: number, opts?: { force?: boolean }) => {
    replayIdxRef.current = idx

    if (!opts?.force) {
      const now = performance.now()
      if (idx === replayCommittedIdxRef.current || now - replayUiLastCommitAtRef.current < 100) {
        return
      }
      replayUiLastCommitAtRef.current = now
    } else {
      replayUiLastCommitAtRef.current = performance.now()
    }

    replayCommittedIdxRef.current = idx
    setReplayIdx(idx)
  }, [])

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
    if (!id || isFetchingMoreRef.current) return false
    const requestOffset = loadedLogCountRef.current
    if (requestOffset >= totalChunksRef.current && totalChunksRef.current !== 0) return false
    isFetchingMoreRef.current = true
    try {
      const res = await fetchLogs(
        id,
        { offset: requestOffset, limit: DEFAULT_LOG_TAIL },
        node ?? undefined
      )
      if (!isMounted.current) return false
      logResizesRef.current = res.resizes
      const encodedChunks = encodeLogChunks(res.chunks)
      if (res.chunks.length > 0) {
        const next = [...logChunksRef.current, ...encodedChunks]
        logChunksRef.current = next
        loadedLogCountRef.current = res.offset + encodedChunks.length
        setScrubberMax(next.length)
      } else {
        loadedLogCountRef.current = res.offset
      }
      if (res.total !== totalChunksRef.current) {
        totalChunksRef.current = res.total
        setTotalChunks(res.total)
      }
      return encodedChunks.length > 0
    } catch {
      return false
    } finally {
      isFetchingMoreRef.current = false
    }
  }, [id, node])

  const fetchMoreLogsRef = useRef<(() => Promise<boolean>) | null>(null)
  useEffect(() => {
    fetchMoreLogsRef.current = fetchMoreLogs
  }, [fetchMoreLogs])

  const stepReplayRef = useRef<((delta: number) => Promise<void>) | null>(null)
  const stepReplay = useCallback(async (delta: number) => {
    if (delta === 0) return

    const currentIdx = replayIdxRef.current
    if (delta < 0) {
      handleScrubRef.current?.(Math.max(0, currentIdx + delta))
      return
    }

    const targetIdx = currentIdx + delta
    if (targetIdx <= logChunksRef.current.length) {
      handleScrubRef.current?.(targetIdx)
      return
    }

    const loaded = await fetchMoreLogsRef.current?.()
    handleScrubRef.current?.(Math.min(targetIdx, loaded ? logChunksRef.current.length : currentIdx))
  }, [])
  useEffect(() => {
    stepReplayRef.current = stepReplay
  }, [stepReplay])

  useEffect(() => {
    const el = termContainerRef.current
    if (!el || mode !== 'logs') return
    const handleWheel = (e: WheelEvent) => {
      if (e.deltaY < 0 && replayIdxRef.current > 0) {
        const step = e.shiftKey ? 50 : 10
        void stepReplayRef.current?.(-step)
      } else if (e.deltaY > 0) {
        const step = e.shiftKey ? 50 : 10
        void stepReplayRef.current?.(step)
      }
    }
    el.addEventListener('wheel', handleWheel, { capture: true, passive: true })
    return () => el.removeEventListener('wheel', handleWheel, true)
  }, [mode])

  useEffect(() => {
    if (mode !== 'logs') return
    const handleKeyUp = (e: KeyboardEvent) => {
      if (e.key === 'PageUp' || e.key === 'ArrowUp' || e.key === 'ArrowLeft') {
        if (replayIdxRef.current > 0) {
          e.preventDefault()
          const step = e.key === 'PageUp' ? 50 : 10
          void stepReplayRef.current?.(-step)
        }
      } else if (e.key === 'PageDown' || e.key === 'ArrowDown' || e.key === 'ArrowRight') {
        e.preventDefault()
        const step = e.key === 'PageDown' ? 50 : 10
        void stepReplayRef.current?.(step)
      }
    }
    window.addEventListener('keyup', handleKeyUp)
    return () => window.removeEventListener('keyup', handleKeyUp)
  }, [mode])

  useEffect(() => {
    if (!id) return
    isMounted.current = true
    let cancelled = false
    // Defer to avoid StrictMode double-fetch
    const raf = requestAnimationFrame(() => {
      if (cancelled) return
      fetchSession(id, node ?? undefined)
        .then((s) => {
          if (!cancelled && isMounted.current) setSession(s)
        })
        .catch(() => {})
    })
    return () => {
      cancelled = true
      cancelAnimationFrame(raf)
      isMounted.current = false
    }
  }, [id, node, reloadTick])

  // Only connect after session metadata is loaded so the info bar has
  // rendered and the terminal container has its final dimensions.  This
  // prevents a post-connect resize that would cause the PTY program (e.g.
  // Python REPL) to redraw its prompt, producing a duplicate cursor line.
  const sessionReady = session !== null

  useEffect(() => {
    if (mode !== 'attach' || !id || !sessionReady) return

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
    // Defer WebSocket creation to requestAnimationFrame so that:
    // 1) StrictMode's synchronous mount→unmount→mount sets `discarded = true`
    //    before the socket is created, preventing duplicate connections.
    // 2) XTerm's own initial rAF (which calls fitAddon.fit()) runs first
    //    (child effects queue before parent effects), ensuring the terminal
    //    is properly sized before we read dimensions and receive the init snapshot.
    const connectRaf = requestAnimationFrame(() => {
      if (discarded) return
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
            disarmAttachIdleAnimation()
          },
          onInit: (data) => {
            if (!gotSnapshot) {
              pushConnectTrace(`init received (${data.length} bytes)`)
              gotSnapshot = true
            }
            lastWsFrameAtRef.current = Date.now()
            enqueueTerminalOutput([data], { reset: true })
          },
          onData: (data) => {
            lastWsFrameAtRef.current = Date.now()
            setSession(
              (session) =>
                ({
                  ...session,
                  lastActivity: new Date(),
                  status: 'running',
                  last_total_bytes: (session?.last_total_bytes ?? 0) + data.length,
                  input_needed: false,
                }) as SessionSummary
            )
            noteAttachInboundData()
            enqueueTerminalOutput([data])
          },
          onModeChanged: () => {
            lastWsFrameAtRef.current = Date.now()
            // Mode changes are tracked server-side; client doesn't need to act.
          },
          onResized: (rows, cols) => {
            lastWsFrameAtRef.current = Date.now()
            // If the PTY was resized to dimensions that don't match our
            // viewport (e.g. a CLI client resized), push our actual size
            // back so the PTY adapts to the web client.
            termRef.current?.resize(cols, rows)
          },
          onSessionEnded: (code) => {
            ended = true
            lastWsFrameAtRef.current = Date.now()
            disarmAttachIdleAnimation()
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
            noteAttachInboundData()
            pushConnectTrace(`server error frame: ${msg}`)
            if (!isMounted.current) return
            termRef.current?.writeln(`\r\n\x1b[31mError: ${msg}\x1b[0m`)
            setWsError(`Server error: ${msg}`)
          },
          onClose: (code, reason) => {
            disarmAttachIdleAnimation()
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
    })

    return () => {
      discarded = true
      cancelAnimationFrame(connectRaf)
      if (reconnectTimerRef.current !== null) {
        clearTimeout(reconnectTimerRef.current)
        reconnectTimerRef.current = null
      }
      if (outputFlushRafRef.current !== null) {
        cancelAnimationFrame(outputFlushRafRef.current)
        outputFlushRafRef.current = null
      }
      outputWriteInFlightRef.current = false
      outputBufferRef.current = []
      pendingResetRef.current = false
      disarmAttachIdleAnimation()
      pushConnectTrace('teardown current websocket')
      socketRef.current?.close()
      socketRef.current = null
      setWsConnected(false)
      setWsConnecting(false)
    }
  }, [
    mode,
    id,
    node,
    sessionReady,
    pushConnectTrace,
    setSearchParams,
    wsReconnectKey,
    enqueueTerminalOutput,
    noteAttachInboundData,
    disarmAttachIdleAnimation,
  ])

  useEffect(() => {
    if (mode !== 'attach') setWsConnecting(false)
  }, [mode])

  useEffect(() => {
    if (mode !== 'logs' || !isTailMode) return
    setTailLimitInput(String(tailLimit ?? termRef.current?.getSize()?.rows ?? 40))
  }, [mode, logsView, isTailMode, tailLimit])

  // iOS PWA: reconnect the WebSocket immediately when the app returns from
  // background. iOS can resume with a stale "connected" socket state before
  // onclose arrives, so force a reconnect on foreground transitions.
  useEffect(() => {
    if (mode !== 'attach') return

    const handleUserActivity = () => {
      noteAttachUserActivity()
    }

    const handleVisibilityState = () => {
      if (document.visibilityState === 'visible') {
        noteAttachUserActivity()
      }
    }

    window.addEventListener('focus', handleUserActivity)
    window.addEventListener('pointerdown', handleUserActivity, true)
    window.addEventListener('keydown', handleUserActivity, true)
    document.addEventListener('focusin', handleUserActivity)
    document.addEventListener('visibilitychange', handleVisibilityState)

    return () => {
      window.removeEventListener('focus', handleUserActivity)
      window.removeEventListener('pointerdown', handleUserActivity, true)
      window.removeEventListener('keydown', handleUserActivity, true)
      document.removeEventListener('focusin', handleUserActivity)
      document.removeEventListener('visibilitychange', handleVisibilityState)
    }
  }, [mode, noteAttachUserActivity])

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

    // Reset common state.
    termRef.current?.reset()
    commitReplayIdx(0, { force: true })
    replayCommittedIdxRef.current = 0
    isReplayingRef.current = false
    setIsReplaying(false)
    setIsPaused(false)
    isPausedRef.current = false
    if (replayTimerRef.current !== null) {
      clearTimeout(replayTimerRef.current)
      replayTimerRef.current = null
    }
    logChunksRef.current = []
    setScrubberMax(0)
    loadedLogCountRef.current = 0
    totalChunksRef.current = 0
    logReplayStateRef.current = initialLogReplayState()
    logResizesRef.current = []
    isFetchingMoreRef.current = false
    setTotalChunks(0)

    let cancelled = false

    if (isTailMode) {
      // Tail mode: fetch raw bytes and write directly to xterm.
      const raf = requestAnimationFrame(() => {
        if (cancelled) return
        const rows = tailLimit ?? termRef.current?.getSize()?.rows ?? 40
        const cols = termRef.current?.getSize()?.cols ?? 80
        fetchLogsTail(id!, rows, cols, node ?? undefined)
          .then((res) => {
            if (cancelled || !isMounted.current) return
            if (res.output.length > 0) {
              termRef.current?.write(normalizeSnapshotOutputForXterm(res.output), () => {
                termRef.current?.scrollToBottom()
              })
            }
            fetchSession(id!, node ?? undefined)
              .then((s) => {
                if (!cancelled && isMounted.current) setSession(s)
              })
              .catch(() => {})
          })
          .catch(() => {})
      })
      return () => {
        cancelled = true
        cancelAnimationFrame(raf)
      }
    }

    // Replay mode: fetch paginated chunks.
    const raf = requestAnimationFrame(() => {
      if (cancelled) return
      fetchLogs(id!, { limit: DEFAULT_LOG_TAIL }, node ?? undefined)
        .then((res) => {
          if (cancelled || !isMounted.current) return
          const encodedChunks = encodeLogChunks(res.chunks)
          logChunksRef.current = encodedChunks
          loadedLogCountRef.current = res.offset + encodedChunks.length
          logResizesRef.current = res.resizes
          totalChunksRef.current = res.total
          setTotalChunks(res.total)
          setScrubberMax(res.chunks.length)
          if (termRef.current) {
            logReplayStateRef.current = replayLogChunks(termRef.current, encodedChunks, res.resizes)
          }
          commitReplayIdx(res.chunks.length, { force: true })
          fetchSession(id!, node ?? undefined)
            .then((s) => {
              if (!cancelled && isMounted.current) setSession(s)
            })
            .catch(() => {})
        })
        .catch(() => {})
    })

    return () => {
      cancelled = true
      cancelAnimationFrame(raf)
    }
  }, [mode, id, node, reloadTick, commitReplayIdx, isTailMode, tailLimit])

  const isScrubbingRef = useRef(false)
  const wasPlayingBeforeScrubRef = useRef(false)

  const handleScrubRef = useRef<((val: number) => void) | null>(null)
  function handleScrub(val: number) {
    if (replayTimerRef.current !== null) {
      clearTimeout(replayTimerRef.current)
      replayTimerRef.current = null
    }
    // Enter paused state when scrubbing so we can resume easily
    isReplayingRef.current = true
    setIsReplaying(true)
    setIsPaused(true)
    isPausedRef.current = true

    commitReplayIdx(val, { force: true })
    if (termRef.current) {
      logReplayStateRef.current = seekLogChunks(
        termRef.current,
        logChunksRef.current,
        logResizesRef.current,
        logReplayStateRef.current,
        val
      )
    }
  }
  handleScrubRef.current = handleScrub

  function startReplay(fromIdx = 0) {
    if (replayTimerRef.current !== null) {
      clearTimeout(replayTimerRef.current)
      replayTimerRef.current = null
    }
    setIsPaused(false)
    isPausedRef.current = false
    isReplayingRef.current = true
    setIsReplaying(true)
    if (fromIdx === 0) {
      commitReplayIdx(0, { force: true })
      logReplayStateRef.current = initialLogReplayState()
      termRef.current?.reset()
    } else if (termRef.current && logReplayStateRef.current.chunkCount !== fromIdx) {
      logReplayStateRef.current = seekLogChunks(
        termRef.current,
        logChunksRef.current,
        logResizesRef.current,
        logReplayStateRef.current,
        fromIdx
      )
      commitReplayIdx(logReplayStateRef.current.chunkCount, { force: true })
    }
    function step() {
      if (isPausedRef.current) {
        replayTimerRef.current = null
        return
      }
      const chunks = logChunksRef.current
      const idx = logReplayStateRef.current.chunkCount
      if (idx >= chunks.length) {
        if (chunks.length < totalChunksRef.current) {
          if (isFetchingMoreRef.current) {
            replayTimerRef.current = window.setTimeout(step, 30)
            return
          }
          const fetchMoreLogs = fetchMoreLogsRef.current
          if (!fetchMoreLogs) {
            replayTimerRef.current = null
            isReplayingRef.current = false
            setIsReplaying(false)
            setIsPaused(false)
            isPausedRef.current = false
            return
          }
          void fetchMoreLogs().then((loaded) => {
            if (!loaded || isPausedRef.current || !isReplayingRef.current) {
              replayTimerRef.current = null
              isReplayingRef.current = false
              setIsReplaying(false)
              setIsPaused(false)
              isPausedRef.current = false
              return
            }
            replayTimerRef.current = window.setTimeout(step, 0)
          })
          return
        }
        replayTimerRef.current = null
        isReplayingRef.current = false
        setIsReplaying(false)
        setIsPaused(false)
        isPausedRef.current = false
        return
      }
      const maxBytesPerFrame = 2560 // 2.5KB strict limit per frame

      // We use playNextBatch which handles byte limits automatically,
      // even splitting large chunks if necessary.
      // This prevents UI freezing on massive log chunks.

      // 2. Perform a SINGLE write operation for the entire batch
      if (termRef.current) {
        // Adjust max bytes based on speed
        // To make low speeds actually feel slow, we scale exponentially
        // 0.5x -> ~128 bytes/frame
        // 1.0x -> ~2.5KB/frame
        // 2.0x -> ~5KB/frame
        let speedMultiplier = replaySpeedRef.current
        if (speedMultiplier < 1) {
          speedMultiplier = speedMultiplier * speedMultiplier * 0.2
        }

        // Ensure at least some progress (32 bytes minimum)
        const adjustedMaxBytes = Math.max(32, Math.round(maxBytesPerFrame * speedMultiplier))

        logReplayStateRef.current = playNextBatch(
          termRef.current,
          chunks,
          logResizesRef.current,
          logReplayStateRef.current,
          adjustedMaxBytes,
          () => {
            if (isPausedRef.current || !isReplayingRef.current) return

            commitReplayIdx(logReplayStateRef.current.chunkCount)

            // Only schedule the next frame AFTER xterm has processed this batch.
            // This guarantees we never flood the renderer or block the UI.
            replayTimerRef.current = window.setTimeout(step, 10)
          }
        )

        termRef.current.scrollToBottom()
      } else {
        // Fallback (should rarely happen)
        commitReplayIdx(logReplayStateRef.current.chunkCount)
        replayTimerRef.current = window.setTimeout(step, 10)
      }
    }
    replayTimerRef.current = window.setTimeout(step, 0)
  }

  function handleSliderChange(val: number[]) {
    if (!isScrubbingRef.current) {
      wasPlayingBeforeScrubRef.current = isReplayingRef.current && !isPausedRef.current
      isScrubbingRef.current = true
    }
    handleScrub(val[0] ?? 0)
  }

  function handleSliderCommit(val: number[]) {
    isScrubbingRef.current = false
    if (wasPlayingBeforeScrubRef.current) {
      startReplay(val[0] ?? 0)
    }
  }

  function setLogsView(view: 'tail' | 'replay') {
    const next = new URLSearchParams(searchParams)
    next.set('mode', 'logs')
    if (view === 'replay') {
      next.set('view', 'replay')
    } else {
      next.delete('view')
    }
    next.set('reload', String(Date.now()))
    setSearchParams(next)
  }

  function handleReplayButton() {
    if (isTailMode) {
      // Switch from tail mode to replay mode and start from beginning.
      setLogsView('replay')
      return
    }
    if (!isReplaying) {
      startReplay(0)
    } else if (!isPaused) {
      setIsPaused(true)
      isPausedRef.current = true
      if (replayTimerRef.current !== null) {
        clearTimeout(replayTimerRef.current)
        replayTimerRef.current = null
      }
    } else {
      setIsPaused(false)
      isPausedRef.current = false
      startReplay(replayIdx)
    }
  }

  function handleSwitchToTail() {
    if (replayTimerRef.current !== null) {
      clearTimeout(replayTimerRef.current)
      replayTimerRef.current = null
    }
    isReplayingRef.current = false
    setIsReplaying(false)
    setIsPaused(false)
    isPausedRef.current = false
    setLogsView('tail')
  }

  function commitTailLimit(value: string) {
    const next = parseInt(value, 10)
    if (!isNaN(next) && next > 0) {
      setTailLimit(next)
      setTailLimitInput(String(next))
      return
    }
    setTailLimitInput(String(tailLimit ?? termRef.current?.getSize()?.rows ?? 40))
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
            <button
              className="font-mono text-sm text-[hsl(var(--foreground))] font-semibold truncate hover:text-[hsl(var(--primary))] transition-colors"
              onClick={() => setShowMetadataDialog(true)}
            >
              {session?.id}
            </button>
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
            {session && (
              <Button size="sm" variant="ghost" onClick={() => setShowNewSessionDialog(true)}>
                <CopyIcon className="h-4 w-4" />
                Run Again
              </Button>
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
                {session && (
                  <DropdownMenuItem onClick={() => setShowNewSessionDialog(true)}>
                    <CopyIcon className="w-4 h-4" />
                    Run Again
                  </DropdownMenuItem>
                )}
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
          <div className="flex flex-wrap items-center gap-x-4 gap-y-1 px-3 sm:px-4 py-1.5 border-b border-[hsl(var(--border))] bg-[hsl(var(--card))]/60 text-[hsl(var(--muted-foreground))] shrink-0 overflow-y-auto h-[38px]">
            <span className="inline-flex min-w-0 items-start gap-2 text-[hsl(var(--foreground))]">
              <CommandLogo command={session.command} size={24} />
              <div
                className={`space-x-2 ${isInfoBarToggled ? '' : 'truncate'}`}
                onClick={() => setIsInfoBarToggled(!isInfoBarToggled)}
              >
                {session?.title && (
                  <span className="break-all text-[hsl(var(--primary))]">{session.title}</span>
                )}
                <span className="break-all">{sessionDisplayName(session)}</span>
                {session.cwd && (
                  <span className="text-[hsl(var(--foreground))] break-all">{session.cwd}</span>
                )}
                <span className="text-[hsl(var(--foreground))]">
                  {formatTimestamp(session.created_at)}
                </span>
                {exitCode !== undefined && (
                  <span>
                    Exit: <span className="text-[hsl(var(--foreground))]">{exitCode ?? '?'}</span>
                  </span>
                )}
                <span>
                  <span className="text-[hsl(var(--foreground))]">
                    {formatByteSize(session.last_total_bytes)}
                  </span>
                </span>
                {session.tags.length > 0 && (
                  <span>
                    Tags: <span className="text-[hsl(var(--foreground))]">{session.tags.join(', ')}</span>
                  </span>
                )}
                {session.pid != null && (
                  <span>
                    PID: <span className="text-[hsl(var(--foreground))]">{session.pid}</span>
                  </span>
                )}
              </div>
            </span>
          </div>
        )}

        {wsError && <div className="text-red-500 text-sm">{wsError}</div>}

        {/* ── Main body ── */}
        <div
          id="main-container"
          className="sm:flex overflow-y-visible sm:overflow-hidden flex-1 min-h-0"
        >
          {/* Terminal area */}
          <div
            className={`flex flex-col flex-1 w-full overflow-hidden ${mode === 'logs' ? 'h-full' : 'h-[calc(100%-72px)] sm:h-full'}`}
          >
            <div
              ref={termContainerRef}
              className="relative flex-1 min-h-0 bg-[hsl(var(--terminal-bg))] py-2 pl-2 pr-0 h-full w-full overflow-x-auto"
            >
              <div
                aria-hidden="true"
                className={`terminal-viewport-idle-overlay ${mode === 'attach' && isAttachViewportIdle ? 'is-active' : ''}`}
              />
              <XTerm
                key={mode === 'logs' ? `logs-${logsView}` : mode}
                ref={termRef}
                autoFit={mode === 'attach' || isTailMode}
                onData={(x) => (mode === 'attach' ? sendInput(x, false) : undefined)}
                onPaste={mode === 'attach' ? handleTerminalPaste : undefined}
                onResize={mode === 'attach' ? handleTermResize : undefined}
                className={`h-full ${mode === 'attach' || isTailMode ? 'min-w-full' : 'w-500'}`}
              />
            </div>

            {/* Tail mode controls */}
            {mode === 'logs' && isTailMode && (
              <div className="flex flex-row items-center gap-2 px-3 sm:px-4 py-2 border-t border-[hsl(var(--border))] bg-[hsl(var(--card))]/80 shrink-0">
                <span className="text-xs text-[hsl(var(--muted-foreground))]">Tail</span>
                <input
                  type="number"
                  className="w-16 h-7 rounded border border-[hsl(var(--border))] bg-[hsl(var(--background))] text-sm text-center px-1 [appearance:textfield] [&::-webkit-outer-spin-button]:appearance-none [&::-webkit-inner-spin-button]:appearance-none"
                  value={tailLimitInput}
                  min={1}
                  max={5000}
                  onChange={(e) => setTailLimitInput(e.target.value)}
                  onBlur={() => commitTailLimit(tailLimitInput)}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter') {
                      commitTailLimit(tailLimitInput)
                    }
                  }}
                  aria-label="Tail line limit"
                />
                <span className="text-xs text-[hsl(var(--muted-foreground))]">lines</span>
                <div className="flex-1" />
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button variant="secondary" size="icon" onClick={handleReplayButton}>
                      <PlayIcon className="h-4 w-4" />
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent>Replay from start</TooltipContent>
                </Tooltip>
              </div>
            )}

            {/* Scrubber (replay mode) */}
            {mode === 'logs' && !isTailMode && scrubberMax > 0 && (
              <div className="flex flex-row gap-2 px-3 sm:px-4 py-2 border-t border-[hsl(var(--border))] bg-[hsl(var(--card))]/80 shrink-0">
                <div className="flex flex-1 items-center gap-2">
                  <Slider
                    className="flex-1"
                    min={0}
                    max={scrubberMax}
                    value={[replayIdx]}
                    onValueChange={handleSliderChange}
                    onValueCommit={handleSliderCommit}
                    aria-label="Replay scrubber"
                  />
                  <span className="hidden sm:inline text-sm text-[hsl(var(--muted-foreground))] tabular-nums whitespace-nowrap">
                    {totalChunks > scrubberMax
                      ? `${replayIdx}/${scrubberMax} loaded (${totalChunks} total)`
                      : `${replayIdx}/${scrubberMax}`}
                  </span>
                </div>
                <div className="flex items-center gap-1.5">
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <Button
                        variant="secondary"
                        size="icon"
                        onClick={() => void stepReplayRef.current?.(-10)}
                      >
                        <ChevronLeftIcon className="h-4 w-4" />
                      </Button>
                    </TooltipTrigger>
                    <TooltipContent>Back 10 chunks</TooltipContent>
                  </Tooltip>
                  <Tooltip>
                    <TooltipTrigger asChild>
                      <Button
                        variant="secondary"
                        size="icon"
                        onClick={() => void stepReplayRef.current?.(10)}
                      >
                        <ChevronRightIcon className="h-4 w-4" />
                      </Button>
                    </TooltipTrigger>
                    <TooltipContent>Forward 10 chunks</TooltipContent>
                  </Tooltip>
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
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button variant="secondary" size="sm" onClick={handleSwitchToTail}>
                          <TrackNextIcon className="h-4 w-4" />
                          Tail
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent>Switch to tail view</TooltipContent>
                    </Tooltip>
                  </div>
                </div>
              </div>
            )}
          </div>

          {mode === 'attach' && (
            <>
              <div className="overflow-hidden rounded-t-md bg-[hsl(var(--card))]/90">
                <AttachPanel
                  sessionId={id ?? ''}
                  sendInput={(x) => sendInput(x, true)}
                  sendBusy={sendBusy}
                  showKeyError={showKeyError}
                  uploadFile={handleUploadFile}
                />
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
        <NewSessionDialog
          open={showNewSessionDialog}
          onClose={() => setShowNewSessionDialog(false)}
          initialValues={session ? buildNewSessionInitialValues(session) : undefined}
          node={node ?? undefined}
        />
        <SessionMetadataDialog
          open={showMetadataDialog}
          session={session}
          node={node ?? undefined}
          onClose={() => setShowMetadataDialog(false)}
          onSaved={(updated) => {
            setSession(updated)
          }}
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
