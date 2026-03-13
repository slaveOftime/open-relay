import { useState, useEffect, useCallback, useMemo, useRef, Fragment } from 'react'
import { useNavigate, useSearchParams } from 'react-router-dom'
import type { ListParams } from '@/api/client'
import {
  SessionSortField,
  SortOrder,
  isSessionStatusFilter,
  isSessionSortField,
  isSortOrder,
  type SessionSummary,
  type SessionStatusFilter,
  type NodeSummary,
} from '@/api/types'
import {
  fetchSessions,
  stopSession,
  killSession,
  subscribeEvents,
  startSession,
  fetchNodes,
} from '@/api/client'
import { NodeSelector } from '@/components/NodeSelector'
import {
  agentName,
  cwdBasename,
  formatTimestamp,
  sessionDisplayName,
  parseArgString,
} from '@/utils/format'
import Logo from '@/components/Logo'
import SseStatusDot from '@/components/SseStatusDot'
import StatusBadge from '@/components/StatusBadge'
import SparklineSvg, { SparklineStore } from '@/components/SparklineSvg'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Badge } from '@/components/ui/badge'
import { Card, CardContent, CardFooter } from '@/components/ui/card'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { ToggleGroup, ToggleGroupItem } from '@/components/ui/toggle-group'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip'
import * as Form from '@radix-ui/react-form'
import {
  ArchiveIcon,
  BellIcon,
  CaretSortIcon,
  ChevronDownIcon,
  ChevronLeftIcon,
  ChevronRightIcon,
  ChevronUpIcon,
  CopyIcon,
  Cross2Icon,
  FileTextIcon,
  Link2Icon,
  MixerHorizontalIcon,
  PlayIcon,
  PlusIcon,
  ReloadIcon,
  StopIcon,
} from '@radix-ui/react-icons'
import {
  disablePushNotifications,
  showSessionNotification,
  syncPushSubscription,
  type PushSetupState,
} from '@/lib/push'
const PREFS_KEY = 'open-relay.webv2.sessions.preferences.v1'
const LEGACY_PREFS_KEY = 'open-relay.sessions.preferences.v1'
const PAGE_SIZE = 15

const sparklines = new SparklineStore()
const sessionPageRequests = new Map<string, Promise<{ items: SessionSummary[]; total: number }>>()

type GroupBy = 'none' | 'cwd' | 'command'

type SessionPrefs = {
  search: string
  statusFilter: SessionStatusFilter
  groupBy: GroupBy
  node: string | null
  sortField: SessionSortField
  sortOrder: SortOrder
}

function normalizeStatusFilter(value: unknown): SessionStatusFilter {
  return isSessionStatusFilter(value) ? value : 'all'
}

function isTerminalStatus(status: SessionSummary['status']): boolean {
  return status === 'stopped' || status === 'killed' || status === 'failed'
}

const SORT_OPTIONS: Array<{ label: string; value: SessionSortField }> = [
  { label: 'Created At', value: SessionSortField.CreatedAt },
  { label: 'Status', value: SessionSortField.Status },
  { label: 'Title', value: SessionSortField.Title },
  { label: 'ID', value: SessionSortField.Id },
  { label: 'Command', value: SessionSortField.Command },
  { label: 'CWD', value: SessionSortField.Cwd },
  { label: 'PID', value: SessionSortField.Pid },
]

function loadSessionPrefs(): SessionPrefs {
  const defaults: SessionPrefs = {
    search: '',
    statusFilter: 'all',
    groupBy: 'none',
    node: null,
    sortField: SessionSortField.CreatedAt,
    sortOrder: SortOrder.Desc,
  }
  if (typeof window === 'undefined') return defaults
  try {
    const raw =
      window.localStorage.getItem(PREFS_KEY) ?? window.localStorage.getItem(LEGACY_PREFS_KEY)
    if (!raw) return defaults
    const parsed = JSON.parse(raw) as Partial<SessionPrefs>
    const groupBy = parsed.groupBy
    const node = parsed.node
    const sortField = parsed.sortField
    const sortOrder = parsed.sortOrder
    return {
      search: typeof parsed.search === 'string' ? parsed.search : defaults.search,
      statusFilter: normalizeStatusFilter(parsed.statusFilter),
      groupBy:
        groupBy === 'none' || groupBy === 'cwd' || groupBy === 'command'
          ? groupBy
          : defaults.groupBy,
      node: normalizeStoredNode(node) ?? defaults.node,
      sortField: isSessionSortField(sortField) ? sortField : defaults.sortField,
      sortOrder: isSortOrder(sortOrder) ? sortOrder : defaults.sortOrder,
    }
  } catch {
    return defaults
  }
}

function saveSessionPrefs(prefs: SessionPrefs) {
  if (typeof window === 'undefined') return
  try {
    window.localStorage.setItem(PREFS_KEY, JSON.stringify(prefs))
  } catch {
    /* ignore */
  }
}

function normalizeStoredNode(value: unknown): string | null {
  if (typeof value !== 'string') return null
  const trimmed = value.trim()
  return trimmed === '' ? null : trimmed
}

function getSessionListRequestKey(params: ListParams): string {
  return JSON.stringify({
    search: params.search ?? '',
    status: params.status ?? '',
    limit: params.limit ?? null,
    offset: params.offset ?? null,
    sort: params.sort ?? '',
    order: params.order ?? '',
    node: params.node ?? '',
  })
}

function fetchSessionsOnce(params: ListParams) {
  const key = getSessionListRequestKey(params)
  const existing = sessionPageRequests.get(key)
  if (existing) return existing

  const request = fetchSessions(params).finally(() => {
    if (sessionPageRequests.get(key) === request) {
      sessionPageRequests.delete(key)
    }
  })
  sessionPageRequests.set(key, request)
  return request
}

function buildSeriesMap(items: SessionSummary[]) {
  items.forEach((session) => {
    sparklines.ensure(session.id)
  })
  return new Map(items.map((session) => [session.id, sparklines.getSeries(session.id)]))
}

function syncSeriesMap(items: SessionSummary[]) {
  return new Map(items.map((session) => [session.id, sparklines.getSeries(session.id)]))
}

function updateSparklineForSession(session: SessionSummary) {
  sparklines.touch(session.id)
}

// ── Skeleton loading ───────────────────────────────────────────────────────

function SkeletonRow() {
  return (
    <tr className="border-b border-[hsl(var(--border))]">
      {[8, 30, 12, 10, 8, 8, 20, 8].map((w, i) => (
        <TableCell key={i} className="px-3 py-3">
          <div
            className="h-3 rounded animate-shimmer"
            style={{ width: `${w + ((i * 7) % 10)}%` }}
          />
        </TableCell>
      ))}
    </tr>
  )
}

function SkeletonCard() {
  return (
    <div className="mx-3 my-2 rounded-xl border border-[hsl(var(--border))] bg-[hsl(var(--card))] p-4 flex flex-col gap-3">
      <div className="flex items-center justify-between">
        <div className="h-4 w-20 rounded-full animate-shimmer" />
        <div className="h-3 w-10 rounded animate-shimmer" />
      </div>
      <div className="h-3.5 rounded animate-shimmer" style={{ width: '60%' }} />
      <div className="flex gap-2">
        <div className="h-3 w-14 rounded animate-shimmer" />
        <div className="h-3 w-12 rounded animate-shimmer" />
      </div>
    </div>
  )
}

// ── Session Row ────────────────────────────────────────────────────────────

function SessionRow({
  session,
  series,
  animateIn,
  onStop,
  onKill,
  onRunAgain,
  node,
}: {
  session: SessionSummary
  series: number[]
  animateIn?: boolean
  onStop: (id: string) => void
  onKill: (id: string) => void
  onRunAgain: (session: SessionSummary) => void
  node?: string
}) {
  const navigate = useNavigate()
  const [pendingAction, setPendingAction] = useState<'stop' | 'kill' | null>(null)
  const isRunning =
    session.status === 'running' || session.status === 'stopping' || session.status === 'created'

  const accentClass = session.input_needed
    ? '[box-shadow:inset_2px_0_0_0_rgb(245_158_11/0.8)] bg-amber-50 dark:bg-amber-950/10'
    : session.status === 'running'
      ? '[box-shadow:inset_2px_0_0_0_rgb(22_163_74/0.5)]'
      : ''

  const rowOpacity = isTerminalStatus(session.status) ? 'opacity-60' : ''
  const animateClass = animateIn ? 'animate-row-slide-in' : ''

  function openSession(mode: 'attach' | 'logs') {
    navigate(
      `/session/${session.id}?mode=${mode}${node ? `&node=${encodeURIComponent(node)}` : ''}`
    )
  }

  return (
    <>
      <TableRow
        className={`group border-b border-[hsl(var(--border))] transition-colors duration-150 hover:bg-[hsl(var(--accent))] cursor-pointer ${rowOpacity} ${animateClass}`}
        onClick={() => openSession(isRunning ? 'attach' : 'logs')}
      >
        {/* ID */}
        <TableCell
          className={`px-3 py-2.5 text-[hsl(var(--muted-foreground))] text-xs font-mono truncate max-w-0 ${accentClass}`}
          onClick={(e) => {
            e.stopPropagation()
            navigator.clipboard.writeText(session.id).catch(() => { })
          }}
        >
          <Tooltip>
            <TooltipTrigger asChild>
              <span>{session.id.slice(0, 7)}</span>
            </TooltipTrigger>
            <TooltipContent>{`${session.id} — click to copy`}</TooltipContent>
          </Tooltip>
        </TableCell>

        {/* Title */}
        <TableCell className="px-3 py-2.5 truncate max-w-0">
          <span className="text-[hsl(var(--foreground))] text-sm group-hover:text-[hsl(var(--primary))] transition-colors">
            {session.title}
          </span>
        </TableCell>

        {/* CMD */}
        <TableCell className="px-3 py-2.5 truncate max-w-0">
          <span className="text-[hsl(var(--foreground))] text-sm group-hover:text-[hsl(var(--primary))] transition-colors">
            {sessionDisplayName(session)}
          </span>
        </TableCell>

        {/* CWD */}
        <TableCell className="px-3 py-2.5 text-[hsl(var(--muted-foreground))] text-xs font-mono truncate max-w-0">
          {session.cwd ? (
            <Tooltip>
              <TooltipTrigger asChild>
                <span>{session.cwd}</span>
              </TooltipTrigger>
              <TooltipContent>{session.cwd}</TooltipContent>
            </Tooltip>
          ) : null}
        </TableCell>

        {/* Status */}
        <TableCell className="px-3 py-2.5 whitespace-nowrap">
          <StatusBadge status={session.status} inputNeeded={session.input_needed} />
        </TableCell>

        {/* Created at */}
        <TableCell className="px-3 py-2.5 text-[hsl(var(--muted-foreground))] text-xs whitespace-nowrap">
          {formatTimestamp(session.created_at)}
        </TableCell>

        {/* Activity */}
        <TableCell className="px-3 py-2.5">
          <SparklineSvg series={series} enableAnimation={(isRunning && (node === null || node === "__local__"))} />
        </TableCell>

        {/* PID */}
        <TableCell className="px-3 py-2.5 text-[hsl(var(--muted-foreground))] text-xs font-mono">
          {session.pid != null && session.pid}
        </TableCell>

        {/* Actions */}
        <TableCell className="px-3 py-2.5" onClick={(e) => e.stopPropagation()}>
          <div className="flex items-center gap-1">
            {isRunning && (
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button variant="link" size="icon" onClick={() => openSession('attach')}>
                    <Link2Icon className="h-4 w-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>Attach</TooltipContent>
              </Tooltip>
            )}
            {isRunning && (
              <>
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      variant="ghost"
                      size="icon"
                      className="text-amber-600 hover:text-amber-600"
                      onClick={() => setPendingAction('stop')}
                    >
                      <StopIcon className="h-4 w-4" />
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent>Stop</TooltipContent>
                </Tooltip>
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      variant="ghost"
                      size="icon"
                      className="text-red-600 hover:text-red-600"
                      onClick={() => setPendingAction('kill')}
                    >
                      <Cross2Icon className="h-4 w-4" />
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent>Kill</TooltipContent>
                </Tooltip>
              </>
            )}
            <Tooltip>
              <TooltipTrigger asChild>
                <Button variant="ghost" size="icon" onClick={() => openSession('logs')}>
                  <FileTextIcon className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Logs</TooltipContent>
            </Tooltip>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button variant="ghost" size="icon" onClick={() => onRunAgain(session)}>
                  <CopyIcon className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Run Again</TooltipContent>
            </Tooltip>
          </div>
        </TableCell>
      </TableRow>

      {/* Confirm dialog */}
      <ConfirmActionDialog
        action={pendingAction}
        sessionId={session.id}
        onConfirm={(action) => {
          if (action === 'stop') onStop(session.id)
          else onKill(session.id)
        }}
        onClose={() => setPendingAction(null)}
      />
    </>
  )
}

// ── Session Card (mobile) ──────────────────────────────────────────────────

function SessionCard({
  session,
  series,
  animateIn,
  onStop,
  onKill,
  onRunAgain,
  node,
}: {
  session: SessionSummary
  series: number[]
  animateIn?: boolean
  onStop: (id: string) => void
  onKill: (id: string) => void
  onRunAgain: (session: SessionSummary) => void
  node?: string
}) {
  const navigate = useNavigate()
  const [pendingAction, setPendingAction] = useState<'stop' | 'kill' | null>(null)
  const isRunning =
    session.status === 'running' || session.status === 'stopping' || session.status === 'created'

  const titleTone =
    isTerminalStatus(session.status) ? 'text-[hsl(var(--foreground))]/70' : 'text-[hsl(var(--foreground))]'
  const animateClass = animateIn ? 'animate-row-slide-in' : ''

  function openSession(mode: 'attach' | 'logs') {
    navigate(
      `/session/${session.id}?mode=${mode}${node ? `&node=${encodeURIComponent(node)}` : ''}`
    )
  }

  return (
    <>
      <Card
        className={`relative rounded-xl shadow-none mx-3 my-2 overflow-hidden flex flex-col transition-colors hover:border-[hsl(var(--border))]/80 ${animateClass}`}
      >
        <CardContent className="px-4 pt-3.5 pb-3 flex flex-col gap-2">
          {/* Row 1: id, status, pid, created at */}
          <div className="flex items-center gap-2 flex-wrap">
            <span className="font-mono text-sm text-[hsl(var(--foreground))] font-semibold">
              {session.id.slice(0, 7)}
            </span>
            <StatusBadge status={session.status} inputNeeded={session.input_needed} />
            <div className="flex-1" />
            <span className="text-xs text-[hsl(var(--muted-foreground))] tabular-nums">
              {formatTimestamp(session.created_at)}
            </span>
          </div>

          {/* Row 2: title */}
          <Button
            variant="ghost"
            className="w-full text-left h-auto py-0.5 justify-start -mx-1 px-1"
            onClick={() => openSession(isRunning ? 'attach' : 'logs')}
          >
            <span className="flex flex-col items-start w-full">
              {session.title && (
                <span className={`block text-sm font-medium truncate leading-snug ${titleTone}`}>
                  {session.title}
                </span>
              )}
              <span className={`block text-base font-medium truncate leading-snug ${titleTone}`}>
                {session.command} {session.args ? session.args.join(' ') : ''}
              </span>
            </span>
          </Button>

          {/* Row 3: cwd */}
          {session.cwd && (
            <div className="flex items-center gap-2">
              <ArchiveIcon className="w-3.5 h-3.5 text-[hsl(var(--muted-foreground))] shrink-0" />
              <Tooltip>
                <TooltipTrigger asChild>
                  <span className="text-sm text-[hsl(var(--muted-foreground))] font-mono truncate">
                    {session.cwd}
                  </span>
                </TooltipTrigger>
                <TooltipContent>{session.cwd}</TooltipContent>
              </Tooltip>
            </div>
          )}

          {/* Row 4: activity sparkline */}
          {session.status === 'running' && (node === null || node === "__local__") && (
            <div className="pt-1 w-full opacity-90">
              <SparklineSvg
                series={series}
                fullWidth
                className="w-full"
                enableAnimation={isRunning}
              />
            </div>
          )}
        </CardContent>

        <div className="border-t border-[hsl(var(--border))]" />

        {/* Action bar */}
        <CardFooter
          className="flex items-center gap-2 px-3.5 py-2 overflow-x-auto"
          onClick={(e) => e.stopPropagation()}
        >
          {isRunning && (
            <Button
              variant="outline"
              className="border-[hsl(var(--primary))] text-[hsl(var(--primary))]"
              size="sm"
              onClick={() => openSession('attach')}
            >
              <Link2Icon className="h-4 w-4" />
              Attach
            </Button>
          )}
          {isRunning && (
            <>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button variant="stop" size="icon" onClick={() => setPendingAction('stop')}>
                    <StopIcon className="h-4 w-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>Stop</TooltipContent>
              </Tooltip>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button variant="kill" size="icon" onClick={() => setPendingAction('kill')}>
                    <Cross2Icon className="h-4 w-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>Kill</TooltipContent>
              </Tooltip>
            </>
          )}
          <Button variant="ghost" size="sm" onClick={() => openSession('logs')}>
            <FileTextIcon className="h-4 w-4" />
            Logs
          </Button>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="icon"
                className="ml-auto shrink-0"
                onClick={() => onRunAgain(session)}
              >
                <CopyIcon className="h-4 w-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>Run Again</TooltipContent>
          </Tooltip>
        </CardFooter>
      </Card>

      <ConfirmActionDialog
        action={pendingAction}
        sessionId={session.id}
        onConfirm={(action) => {
          if (action === 'stop') onStop(session.id)
          else onKill(session.id)
        }}
        onClose={() => setPendingAction(null)}
      />
    </>
  )
}

// ── Confirm Action Dialog ──────────────────────────────────────────────────

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
              Are you sure you want to <span className="text-red-500 font-semibold">kill</span>{' '}
              session{' '}
              <span className="font-mono text-[hsl(var(--foreground))]">
                {sessionId.slice(0, 7)}
              </span>
              ? The process will be terminated immediately.
            </>
          ) : (
            <>
              Are you sure you want to <span className="text-amber-500 font-semibold">stop</span>{' '}
              session{' '}
              <span className="font-mono text-[hsl(var(--foreground))]">
                {sessionId.slice(0, 7)}
              </span>
              ? A graceful shuTableCellown signal will be sent.
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

// ── New Session Dialog ─────────────────────────────────────────────────────

type NewSessionInitialValues = { cmd: string; args: string; title: string; cwd: string }

function NewSessionDialog({
  open,
  onClose,
  initialValues,
  node,
}: {
  open: boolean
  onClose: () => void
  initialValues?: NewSessionInitialValues
  node?: string
}) {
  void useNavigate
  const [cmd, setCmd] = useState('')
  const [args, setArgs] = useState('')
  const [title, setTitle] = useState('')
  const [cwd, setCwd] = useState('')
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (open && initialValues) {
      setCmd(initialValues.cmd)
      setArgs(initialValues.args)
      setTitle(initialValues.title)
      setCwd(initialValues.cwd)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  async function handleSubmit() {
    if (!cmd.trim()) {
      setError('Command is required')
      return
    }
    setLoading(true)
    setError(null)
    try {
      const argList = args.trim() ? parseArgString(args.trim()) : []
      await startSession({
        cmd: cmd.trim(),
        args: argList,
        title: title.trim() || undefined,
        cwd: cwd.trim() || undefined,
        node: node ?? undefined,
      })
      onClose()
      resetForm()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to start session')
    } finally {
      setLoading(false)
    }
  }

  function resetForm() {
    setCmd('')
    setArgs('')
    setTitle('')
    setCwd('')
    setError(null)
  }

  function handleClose() {
    resetForm()
    onClose()
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(open) => {
        if (!open) handleClose()
      }}
    >
      <DialogContent className="max-w-md">
        <DialogHeader>
          <DialogTitle>New Session</DialogTitle>
        </DialogHeader>
        <Form.Root
          onSubmit={(e) => {
            e.preventDefault()
            void handleSubmit()
          }}
          className="flex flex-col gap-3 mt-1"
        >
          <Form.Field name="command" className="flex flex-col gap-1.5">
            <Form.Label className="text-xs text-[hsl(var(--muted-foreground))]">
              Command <span className="text-red-500">*</span>
            </Form.Label>
            <Form.Control asChild>
              <Input
                value={cmd}
                onChange={(e) => setCmd(e.target.value)}
                placeholder="claude, bash, python…"
                required
                autoFocus
              />
            </Form.Control>
            <Form.Message match="valueMissing" className="text-red-500 text-xs">
              Command is required
            </Form.Message>
          </Form.Field>
          <Form.Field name="arguments" className="flex flex-col gap-1.5">
            <Form.Label className="text-xs text-[hsl(var(--muted-foreground))]">
              Arguments
            </Form.Label>
            <Form.Control asChild>
              <Input
                value={args}
                onChange={(e) => setArgs(e.target.value)}
                placeholder="--model sonnet-3.7 (space-separated)"
              />
            </Form.Control>
          </Form.Field>
          <Form.Field name="title" className="flex flex-col gap-1.5">
            <Form.Label className="text-xs text-[hsl(var(--muted-foreground))]">Title</Form.Label>
            <Form.Control asChild>
              <Input
                value={title}
                onChange={(e) => setTitle(e.target.value)}
                placeholder="Optional display name"
              />
            </Form.Control>
          </Form.Field>
          <Form.Field name="cwd" className="flex flex-col gap-1.5">
            <Form.Label className="text-xs text-[hsl(var(--muted-foreground))]">
              Working Directory
            </Form.Label>
            <Form.Control asChild>
              <Input
                value={cwd}
                onChange={(e) => setCwd(e.target.value)}
                placeholder="/path/to/project"
              />
            </Form.Control>
          </Form.Field>
          {error && <p className="text-red-500 text-xs">{error}</p>}
          <div className="flex justify-end gap-2 pt-1">
            <Button type="button" variant="ghost" size="sm" onClick={handleClose}>
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={loading}>
              {loading ? 'Starting…' : 'Start Session'}
            </Button>
          </div>
        </Form.Root>
      </DialogContent>
    </Dialog>
  )
}

// ── Sort indicator ─────────────────────────────────────────────────────────

function SortIcon({
  field,
  sortField,
  sortOrder,
}: {
  field: SessionSortField
  sortField: SessionSortField
  sortOrder: SortOrder
}) {
  if (field !== sortField) return <CaretSortIcon className="w-3 h-3 opacity-40" />
  return sortOrder === SortOrder.Asc ? (
    <ChevronUpIcon className="w-3 h-3" />
  ) : (
    <ChevronDownIcon className="w-3 h-3" />
  )
}

// ── Empty state ────────────────────────────────────────────────────────────

function EmptyState({ onNewSession }: { onNewSession: () => void }) {
  return (
    <div className="flex flex-col items-center justify-center py-24 text-[hsl(var(--muted-foreground))] gap-3">
      <Logo size={80} />
      <p className="text-sm text-[hsl(var(--muted-foreground))]">No sessions yet.</p>
      <Button size="sm" onClick={onNewSession}>
        <PlusIcon className="w-4 h-4" />
        New Session
      </Button>
    </div>
  )
}

// ── Main page ──────────────────────────────────────────────────────────────

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
  const [page, setPage] = useState(0)
  const [showNewSession, setShowNewSession] = useState(false)
  const [rerunSession, setRerunSession] = useState<SessionSummary | null>(null)
  const [sseStatus, setSseStatus] = useState<'live' | 'reconnecting' | 'offline'>(
    typeof navigator !== 'undefined' && !navigator.onLine ? 'offline' : 'reconnecting'
  )
  const [seriesMap, setSeriesMap] = useState<Map<string, number[]>>(new Map())
  const [enteringIds, setEnteringIds] = useState<Set<string>>(new Set())
  const [showFilters, setShowFilters] = useState(false)
  const [pushState, setPushState] = useState<PushSetupState>('idle')

  const enterAnimTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const isMounted = useRef(true)
  const prevIdsRef = useRef<Set<string>>(new Set())
  const hasLoadedRef = useRef(false)
  const pushStateRef = useRef<PushSetupState>('idle')

  useEffect(() => {
    pushStateRef.current = pushState
  }, [pushState])

  const applySessionItems = useCallback((items: SessionSummary[]) => {
    setSessions(items)
    setSeriesMap(buildSeriesMap(items))
  }, [])

  const loadLocal = useCallback(
    async (opts?: { background?: boolean }) => {
      if (selectedNode) return

      const shouldShowSkeleton = !opts?.background && !hasLoadedRef.current
      if (shouldShowSkeleton) setLoading(true)
      else setRefreshing(true)

      try {
        const params: ListParams = {
          search: search || undefined,
          status: statusFilter === 'all' ? undefined : statusFilter,
          limit: PAGE_SIZE,
          offset: page * PAGE_SIZE,
          sort: sortField,
          order: sortOrder,
        }
        const res = await fetchSessionsOnce(params)
        if (!isMounted.current) return
        if (selectedNode) return

        hasLoadedRef.current = true
        applySessionItems(res.items)
        setRemoteTotal(res.total)
      } catch {
        /* ignore */
      } finally {
        if (isMounted.current) {
          if (shouldShowSkeleton) setLoading(false)
          setRefreshing(false)
        }
      }
    },
    [applySessionItems, page, search, selectedNode, sortField, sortOrder, statusFilter]
  )

  const loadRemote = useCallback(
    async (opts?: { background?: boolean }) => {
      if (!selectedNode) return

      const shouldShowSkeleton = !opts?.background && !hasLoadedRef.current
      if (shouldShowSkeleton) setLoading(true)
      else setRefreshing(true)

      try {
        const params: ListParams = {
          search: search || undefined,
          status: statusFilter === 'all' ? undefined : statusFilter,
          limit: PAGE_SIZE,
          offset: page * PAGE_SIZE,
          sort: sortField,
          order: sortOrder,
          node: selectedNode,
        }
        const res = await fetchSessionsOnce(params)
        if (!isMounted.current || !selectedNode) return

        hasLoadedRef.current = true
        applySessionItems(res.items)
        setRemoteTotal(res.total)
      } catch {
        /* ignore */
      } finally {
        if (isMounted.current) {
          if (shouldShowSkeleton) setLoading(false)
          setRefreshing(false)
        }
      }
    },
    [applySessionItems, page, search, selectedNode, sortField, sortOrder, statusFilter]
  )

  const reloadSessions = useCallback(
    async (opts?: { background?: boolean }) => {
      fetchNodes().catch(() => { })
      if (selectedNode) {
        await loadRemote(opts)
        return
      }
      await loadLocal(opts)
    },
    [loadLocal, loadRemote, selectedNode]
  )

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
      .catch(() => { })
  }, [])

  useEffect(() => {
    isMounted.current = true
    return () => {
      isMounted.current = false
    }
  }, [])

  // Periodically advance sparkline buckets so activity decays visually
  useEffect(() => {
    const id = setInterval(() => {
      if (!isMounted.current) return
      setSessions((prev) => {
        setSeriesMap(syncSeriesMap(prev))
        return prev
      })
    }, 2_000)
    return () => clearInterval(id)
  }, [])

  useEffect(() => {
    const handleOnline = () => setSseStatus('reconnecting')
    const handleOffline = () => setSseStatus('offline')
    window.addEventListener('online', handleOnline)
    window.addEventListener('offline', handleOffline)
    return () => {
      window.removeEventListener('online', handleOnline)
      window.removeEventListener('offline', handleOffline)
    }
  }, [])

  useEffect(() => {
    saveSessionPrefs({ search, statusFilter, groupBy, node: selectedNode, sortField, sortOrder })
  }, [search, selectedNode, statusFilter, groupBy, sortField, sortOrder])

  useEffect(() => {
    if (!selectedNode) {
      void loadLocal()
    }
  }, [loadLocal, selectedNode])

  useEffect(() => {
    if (selectedNode) {
      void loadRemote()
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

  useEffect(() => {
    const cleanup = subscribeEvents((ev) => {
      if (ev.event === 'snapshot') {
        if (selectedNode) return
        void reloadSessions({ background: true })
        return
      }
      if (ev.event === 'session_created') {
        if (selectedNode) return
        updateSparklineForSession(ev.data)
        void reloadSessions({ background: true })
        return
      }
      if (ev.event === 'session_updated') {
        if (selectedNode) return
        updateSparklineForSession(ev.data)
        void reloadSessions({ background: true })
        return
      }
      if (ev.event === 'session_deleted') {
        if (selectedNode) return
        sparklines.remove(ev.data.id)
        void reloadSessions({ background: true })
        return
      }
      if (ev.event === 'session_notification') {
        if (pushStateRef.current === 'subscribed') return
        const tag = ev.data.session_ids[0] ?? 'open-relay-session-notification'
        void showSessionNotification(ev.data.summary, ev.data.body, tag)
        return
      }
    }, setSseStatus)
    return () => {
      cleanup()
      if (enterAnimTimerRef.current) clearTimeout(enterAnimTimerRef.current)
    }
  }, [reloadSessions, selectedNode])

  const pagedSessions = sessions

  const total = remoteTotal

  useEffect(() => {
    const lastPage = Math.max(Math.ceil(total / PAGE_SIZE) - 1, 0)
    setPage((prev) => Math.min(prev, lastPage))
  }, [total])

  const grouped = useMemo<Array<{ key: string; items: SessionSummary[] }>>(() => {
    if (groupBy === 'none') return [{ key: '', items: pagedSessions }]
    if (groupBy === 'cwd') {
      const map = new Map<string, SessionSummary[]>()
      for (const s of pagedSessions) {
        const k = cwdBasename(s.cwd) || '(no cwd)'
        if (!map.has(k)) map.set(k, [])
        map.get(k)!.push(s)
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

  function handleRunAgain(session: SessionSummary) {
    setRerunSession(session)
    setShowNewSession(true)
  }

  function handleNodeChange(node: string | null) {
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

  async function handleStop(id: string) {
    await stopSession(id, undefined, selectedNode ?? undefined).catch(() => { })
    if (selectedNode) void loadRemote()
  }
  async function handleKill(id: string) {
    await killSession(id, selectedNode ?? undefined).catch(() => { })
    if (selectedNode) void loadRemote()
  }

  const totalPages = Math.ceil(total / PAGE_SIZE)

  function handleSort(field: SessionSortField) {
    let nextSortField = sortField
    let nextSortOrder = sortOrder
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
    })
    setPage(0)
  }

  const hasActiveFilters =
    search !== '' ||
    statusFilter !== 'all' ||
    groupBy !== 'none' ||
    sortField !== SessionSortField.CreatedAt ||
    sortOrder !== SortOrder.Desc

  const statusChips: { label: string; value: SessionStatusFilter }[] = [
    { label: 'All', value: 'all' },
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

  return (
    <TooltipProvider>
      <div className="flex flex-col h-full bg-[hsl(var(--background))] text-[hsl(var(--foreground))]">
        {/* ── Header ── */}
        <header className="border-b border-[hsl(var(--border))] bg-[hsl(var(--background))]/95 sticky top-0 z-30 backdrop-blur">
          {/* Mobile row */}
          <div className="flex flex-wrap items-center gap-2 px-3 py-2 md:hidden">
            <div
              className="flex items-center gap-2 text-[hsl(var(--primary))] font-bold text-lg cursor-pointer min-w-0"
              onClick={() => void reloadSessions({ background: true })}
            >
              <Logo />
              <span className="truncate">Open Relay</span>
            </div>
            <div className="flex-1 min-w-0" />
            <Button
              variant="ghost"
              size="icon"
              onClick={() => void reloadSessions({ background: true })}
              disabled={loading || refreshing}
              aria-label="Refresh sessions"
            >
              <ReloadIcon className="h-4 w-4" />
            </Button>
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
              <Input
                placeholder="Search sessions…"
                value={search}
                onChange={(e) => {
                  setSearch(e.target.value)
                  setPage(0)
                }}
              />
              <div className="flex gap-2">
                <Select value={groupBy} onValueChange={(v) => setGroupBy(v as GroupBy)}>
                  <SelectTrigger className="flex-1 h-8 text-xs">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="none">No grouping</SelectItem>
                    <SelectItem value="cwd">Group by CWD</SelectItem>
                    <SelectItem value="command">Group by agent</SelectItem>
                  </SelectContent>
                </Select>
                <Select
                  value={statusFilter}
                  onValueChange={(v) => {
                    if (isSessionStatusFilter(v)) {
                      setStatusFilter(v)
                      setPage(0)
                    }
                  }}
                >
                  <SelectTrigger className="flex-1 h-8 text-xs">
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
              <NodeSelector
                nodes={nodes}
                selected={selectedNode}
                onChange={handleNodeChange}
                className="w-full"
              />
            </div>
          </div>

          {/* Desktop row */}
          <div className="hidden md:flex flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2.5">
            <div
              className="flex items-center gap-1 text-[hsl(var(--primary))] font-bold text-lg cursor-pointer"
              onClick={() => void reloadSessions({ background: true })}
            >
              <Logo />
              <span>Open Relay</span>
            </div>

            <Input
              className="w-48 h-8 text-sm"
              placeholder="Search sessions…"
              value={search}
              onChange={(e) => {
                setSearch(e.target.value)
                setPage(0)
              }}
            />

            <Select value={groupBy} onValueChange={(v) => setGroupBy(v as GroupBy)}>
              <SelectTrigger className="w-40 h-8 text-sm">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="none">No grouping</SelectItem>
                <SelectItem value="cwd">Group by CWD</SelectItem>
                <SelectItem value="command">Group by agent</SelectItem>
              </SelectContent>
            </Select>

            {/* Status filter (responsive) */}
            <Select
              value={statusFilter}
              onValueChange={(v) => {
                if (isSessionStatusFilter(v)) {
                  setStatusFilter(v)
                  setPage(0)
                }
              }}
            >
              <SelectTrigger className="h-8 w-36 text-sm xl:hidden">
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

            <div className="hidden xl:flex items-center gap-1.5">
              <ToggleGroup
                type="single"
                value={statusFilter}
                onValueChange={(v) => {
                  if (isSessionStatusFilter(v)) {
                    setStatusFilter(v)
                    setPage(0)
                  }
                }}
              >
                {statusChips.map((chip) => (
                  <ToggleGroupItem key={chip.value} value={chip.value}>
                    {chip.label}
                  </ToggleGroupItem>
                ))}
              </ToggleGroup>
            </div>

            <div className="flex-1" />

            <NodeSelector nodes={nodes} selected={selectedNode} onChange={handleNodeChange} />

            <Button
              size="sm"
              variant="ghost"
              onClick={() => void reloadSessions({ background: true })}
              disabled={loading || refreshing}
            >
              <ReloadIcon className="h-4 w-4" />
              <span className="hidden xl:inline">Refresh</span>
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
        <div className="flex-1 md:hidden">
          {loading &&
            sessions.length === 0 &&
            Array.from({ length: 5 }).map((_, i) => <SkeletonCard key={i} />)}
          {!loading && sessions.length === 0 && (
            <EmptyState onNewSession={() => setShowNewSession(true)} />
          )}
          {!loading && sessions.length > 0 && (
            <div className="pb-4">
              {grouped.map(({ key, items }) => (
                <div key={key || '__flat__'}>
                  {groupBy !== 'none' && key && (
                    <div className="px-4 py-1.5 text-xs text-[hsl(var(--muted-foreground))] font-medium bg-[hsl(var(--card))]/40 border-b border-t border-[hsl(var(--border))]">
                      {key}
                    </div>
                  )}
                  {items.map((s) => (
                    <SessionCard
                      key={s.id}
                      session={s}
                      series={seriesMap.get(s.id) ?? []}
                      animateIn={enteringIds.has(s.id)}
                      onStop={handleStop}
                      onKill={handleKill}
                      onRunAgain={handleRunAgain}
                      node={selectedNode ?? undefined}
                    />
                  ))}
                </div>
              ))}
            </div>
          )}
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
            <EmptyState onNewSession={() => setShowNewSession(true)} />
          )}
          {!loading && sessions.length > 0 && (
            <Table className="w-full border-collapse table-fixed">
              <colgroup>
                <col style={{ width: '5rem' }} />
                <col style={{ width: 'fit' }} />
                <col style={{ width: 'fit' }} />
                <col style={{ width: 'auto' }} />
                <col style={{ width: '8rem' }} />
                <col style={{ width: '10rem' }} />
                <col style={{ width: '6rem' }} />
                <col style={{ width: '4rem' }} />
                <col style={{ width: 'fit' }} />
              </colgroup>
              <TableHeader>
                <TableRow>
                  {(
                    [
                      { key: 'id', label: 'ID', sortField: SessionSortField.Id },
                      { key: 'title', label: 'Title', sortField: SessionSortField.Title },
                      {
                        key: 'command',
                        label: 'Command',
                        sortField: SessionSortField.Command,
                      },
                      { key: 'cwd', label: 'CWD', sortField: SessionSortField.Cwd },
                      { key: 'status', label: 'Status', sortField: SessionSortField.Status },
                      {
                        key: 'created_at',
                        label: 'Created At',
                        sortField: SessionSortField.CreatedAt,
                      },
                      { key: 'activity', label: 'Activity', sortField: undefined },
                      { key: 'pid', label: 'PID', sortField: SessionSortField.Pid },
                      { key: 'actions', label: 'Actions', sortField: undefined },
                    ] as const
                  ).map((col) => (
                    <TableHead
                      key={col.key}
                      className={`px-3 py-2.5 text-left text-xs font-medium uppercase tracking-wide border-b border-[hsl(var(--border))] bg-[hsl(var(--background))] sticky z-20 select-none whitespace-nowrap ${col.sortField
                          ? 'cursor-pointer hover:text-[hsl(var(--foreground))] transition-colors'
                          : 'text-[hsl(var(--muted-foreground))]'
                        } ${col.sortField === sortField ? 'text-[hsl(var(--primary))]' : 'text-[hsl(var(--muted-foreground))]'}`}
                      onClick={col.sortField ? () => handleSort(col.sortField!) : undefined}
                    >
                      <span className="inline-flex items-center gap-1">
                        {col.label}
                        {col.sortField && (
                          <SortIcon
                            field={col.sortField}
                            sortField={sortField}
                            sortOrder={sortOrder}
                          />
                        )}
                      </span>
                    </TableHead>
                  ))}
                </TableRow>
              </TableHeader>
              <TableBody>
                {grouped.map(({ key, items }) => (
                  <Fragment key={key || '__flat__'}>
                    {groupBy !== 'none' && key && (
                      <TableRow>
                        <TableCell
                          colSpan={8}
                          className="px-3 py-1.5 text-xs text-[hsl(var(--muted-foreground))] font-medium bg-[hsl(var(--card))]/40 border-b border-[hsl(var(--border))]"
                        >
                          {key}
                        </TableCell>
                      </TableRow>
                    )}
                    {items.map((s) => (
                      <SessionRow
                        key={`${s.id}:${s.status}:${s.input_needed ? 'input' : 'normal'}`}
                        session={s}
                        series={seriesMap.get(s.id) ?? []}
                        animateIn={enteringIds.has(s.id)}
                        onStop={handleStop}
                        onKill={handleKill}
                        onRunAgain={handleRunAgain}
                        node={selectedNode ?? undefined}
                      />
                    ))}
                  </Fragment>
                ))}
              </TableBody>
            </Table>
          )}
        </div>

        {/* ── Meta bar ── */}
        <div className="flex items-center gap-2 px-4 py-2 border-t border-[hsl(var(--border))] bg-[hsl(var(--background))]/80 text-sm text-[hsl(var(--muted-foreground))]">
          <SseStatusDot status={sseStatus} />
          {refreshing && !loading && (
            <span className="text-[hsl(var(--muted-foreground))]">Refreshing…</span>
          )}
          <div className="flex-1"></div>
          <span className="text-sm">
            {PAGE_SIZE} / {total}
          </span>
          <div />
          {totalPages > 1 && (
            <div className="flex items-center gap-0.5">
              <Button
                variant="ghost"
                size="icon"
                disabled={page === 0}
                onClick={() => setPage((p) => p - 1)}
              >
                <ChevronLeftIcon className="h-4 w-4" />
              </Button>
              <span className="px-2 text-sm">
                {page + 1} / {totalPages}
              </span>
              <Button
                variant="ghost"
                size="icon"
                disabled={page >= totalPages - 1}
                onClick={() => setPage((p) => p + 1)}
              >
                <ChevronRightIcon className="h-4 w-4" />
              </Button>
            </div>
          )}
        </div>

        <NewSessionDialog
          open={showNewSession}
          onClose={() => {
            setShowNewSession(false)
            setRerunSession(null)
            void reloadSessions({ background: true })
          }}
          initialValues={
            rerunSession
              ? {
                cmd: rerunSession.command,
                args: rerunSession.args
                  .map((a) => (/\s/.test(a) ? `"${a.replace(/"/g, '\\"')}"` : a))
                  .join(' '),
                title: rerunSession.title ?? '',
                cwd: rerunSession.cwd ?? '',
              }
              : undefined
          }
          node={selectedNode ?? undefined}
        />
      </div>
    </TooltipProvider>
  )
}

// Needed for Badge import
export { Badge }
