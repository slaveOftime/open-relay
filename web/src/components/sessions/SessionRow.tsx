/**
 * One session as a desktop table row.
 *
 * The row keeps its own "stop"/"kill" pending state so a confirm dialog can
 * open without lifting the interaction out of the row, and renders only the
 * columns the user configured (see `pages/sessions-table-columns`).
 */

import { memo, useState } from 'react'
import { Link, useNavigate } from 'react-router-dom'

import type { SessionSummary } from '@/api/types'
import CommandLogo from '@/components/CommandLogo'
import SessionActivitySparkline from '@/components/sparkline/SessionActivitySparkline'
import SessionActionConfirmDialog from '@/components/dialogs/SessionActionConfirmDialog'
import StatusBadge from '@/components/StatusBadge'
import { SessionNotificationButton } from '@/components/sessions/SessionNotificationButton'
import { SessionPinButton } from '@/components/sessions/SessionPinButton'
import { SessionTagList } from '@/components/sessions/SessionTagList'
import { buildSessionHref } from '@/components/sessions/session-href'
import { Button } from '@/components/ui/button'
import { TableCell, TableRow } from '@/components/ui/table'
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'
import {
  CopyIcon,
  Cross2Icon,
  FileTextIcon,
  Link2Icon,
  StopIcon,
  TrashIcon,
} from '@radix-ui/react-icons'
import {
  formatByteSize,
  formatTimestamp,
  normalizeCwdPath,
  sessionDisplayName,
} from '@/utils/format'
import { isTerminalStatus } from '@/utils/session-status'
import type { SessionTableColumn, SessionTableColumnKey } from '@/lib/sessions-table-columns'

export const SessionRow = memo(function SessionRow({
  session,
  animateIn,
  pinned,
  onStop,
  onKill,
  onToggleNotifications,
  onTogglePin,
  onRunAgain,
  onEditSession,
  onRequestDelete,
  notificationsPending,
  node,
  columns,
}: {
  session: SessionSummary
  animateIn?: boolean
  pinned?: boolean
  onStop: (session: SessionSummary) => void
  onKill: (session: SessionSummary) => void
  onToggleNotifications: (session: SessionSummary) => void
  onTogglePin: (session: SessionSummary) => void
  onRunAgain: (session: SessionSummary) => void
  onEditSession: (session: SessionSummary) => void
  onRequestDelete: (session: SessionSummary) => void
  notificationsPending?: boolean
  node?: string
  columns: SessionTableColumn[]
}) {
  const navigate = useNavigate()
  const [pendingAction, setPendingAction] = useState<'stop' | 'kill' | null>(null)
  const isRunning =
    session.status === 'running' || session.status === 'stopping' || session.status === 'created'
  const attachHref = buildSessionHref(session.id, 'attach', node)
  const logsHref = buildSessionHref(session.id, 'logs', node)

  const accentClass = session.input_needed
    ? '[box-shadow:inset_2px_0_0_0_rgb(245_158_11/0.8)] bg-amber-50 dark:bg-amber-950/10'
    : session.status === 'running'
      ? '[box-shadow:inset_2px_0_0_0_rgb(22_163_74/0.5)]'
      : ''

  const rowOpacity = isTerminalStatus(session.status) ? 'opacity-60' : ''
  const animateClass = animateIn ? 'animate-row-slide-in' : ''

  function openSession(mode: 'attach' | 'logs') {
    navigate(buildSessionHref(session.id, mode, node))
  }

  function renderCell(columnKey: SessionTableColumnKey) {
    switch (columnKey) {
      case 'id':
        return (
          <TableCell
            key={columnKey}
            className={`px-3 py-1 text-[hsl(var(--muted-foreground))] text-xs font-mono truncate max-w-0 ${accentClass}`}
            onClick={(e) => {
              e.stopPropagation()
              onEditSession(session)
            }}
          >
            <Tooltip>
              <TooltipTrigger asChild>
                <button className="truncate text-left hover:text-[hsl(var(--primary))] transition-colors">
                  {session.id.slice(0, 7)}
                </button>
              </TooltipTrigger>
              <TooltipContent>{`${session.id} — click to edit`}</TooltipContent>
            </Tooltip>
          </TableCell>
        )
      case 'output':
        return (
          <TableCell key={columnKey} className="px-3 py-1 truncate max-w-0">
            <span className="block truncate text-[hsl(var(--foreground))] text-sm group-hover:text-[hsl(var(--primary))] transition-colors">
              {formatByteSize(session.last_total_bytes)}
            </span>
          </TableCell>
        )
      case 'title':
        return (
          <TableCell key={columnKey} className="px-3 py-1 truncate max-w-0">
            <span className="block truncate text-[hsl(var(--foreground))] text-sm group-hover:text-[hsl(var(--primary))] transition-colors">
              {session.title?.trim() || '—'}
            </span>
          </TableCell>
        )
      case 'tags':
        return (
          <TableCell key={columnKey} className="px-3 py-2 align-middle">
            <SessionTagList tags={session.tags} emptyLabel="—" className="flex-wrap gap-1" />
          </TableCell>
        )
      case 'command':
        return (
          <TableCell key={columnKey} className="px-3 py-1 truncate max-w-0">
            <span className="flex min-w-0 items-center gap-2 text-[hsl(var(--foreground))] text-sm group-hover:text-[hsl(var(--primary))] transition-colors">
              <CommandLogo command={session.command} size={24} />
              <span className="truncate">{sessionDisplayName(session)}</span>
            </span>
          </TableCell>
        )
      case 'cwd':
        return (
          <TableCell
            key={columnKey}
            className="px-3 py-1 text-[hsl(var(--muted-foreground))] text-xs font-mono truncate max-w-0"
          >
            {session.cwd ? (
              <Tooltip>
                <TooltipTrigger asChild>
                  <span>{normalizeCwdPath(session.cwd)}</span>
                </TooltipTrigger>
                <TooltipContent>{normalizeCwdPath(session.cwd)}</TooltipContent>
              </Tooltip>
            ) : null}
          </TableCell>
        )
      case 'status':
        return (
          <TableCell key={columnKey} className="px-3 py-1 whitespace-nowrap">
            <StatusBadge status={session.status} inputNeeded={session.input_needed} />
          </TableCell>
        )
      case 'created_at':
        return (
          <TableCell
            key={columnKey}
            className="px-3 py-1 text-[hsl(var(--muted-foreground))] text-xs whitespace-nowrap"
          >
            {formatTimestamp(session.created_at)}
          </TableCell>
        )
      case 'activity':
        return (
          <TableCell key={columnKey} className="px-3 py-1">
            <SessionActivitySparkline
              sessionId={session.id}
              node={node}
              isRunning={isRunning}
              fullWidth
            />
          </TableCell>
        )
      case 'pid':
        return (
          <TableCell
            key={columnKey}
            className="px-3 py-1 text-[hsl(var(--muted-foreground))] text-xs font-mono"
          >
            {session.pid != null && session.pid}
          </TableCell>
        )
      case 'actions':
        return (
          <TableCell key={columnKey} className="px-3 py-1" onClick={(e) => e.stopPropagation()}>
            <div className="flex items-center gap-1 overflow-x-auto">
              {isRunning && (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button asChild variant="link" size="icon" className="shrink-0">
                      <Link to={attachHref} aria-label="Attach">
                        <Link2Icon className="h-4 w-4" />
                      </Link>
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
                        variant="stop"
                        size="icon"
                        className="shrink-0"
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
                        variant="kill"
                        size="icon"
                        className="shrink-0"
                        onClick={() => setPendingAction('kill')}
                      >
                        <Cross2Icon className="h-4 w-4" />
                      </Button>
                    </TooltipTrigger>
                    <TooltipContent>Kill</TooltipContent>
                  </Tooltip>
                  <SessionNotificationButton
                    enabled={session.notifications_enabled}
                    disabled={!isRunning}
                    pending={notificationsPending}
                    onToggle={() => onToggleNotifications(session)}
                  />
                  <SessionPinButton
                    pinned={pinned ?? false}
                    onToggle={() => onTogglePin(session)}
                  />
                </>
              )}
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button asChild variant="ghost" size="icon" className="shrink-0">
                    <Link to={logsHref} aria-label="Logs">
                      <FileTextIcon className="h-4 w-4" />
                    </Link>
                  </Button>
                </TooltipTrigger>
                <TooltipContent>Logs</TooltipContent>
              </Tooltip>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    size="icon"
                    className="shrink-0"
                    onClick={() => onRunAgain(session)}
                    aria-label="Run Again"
                  >
                    <CopyIcon className="h-4 w-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>Run Again</TooltipContent>
              </Tooltip>
              <Tooltip>
                <TooltipTrigger asChild>
                  <Button
                    variant="stop"
                    size="icon"
                    className="shrink-0"
                    onClick={() => onRequestDelete(session)}
                    aria-label="Delete session"
                  >
                    <TrashIcon className="h-4 w-4" />
                  </Button>
                </TooltipTrigger>
                <TooltipContent>Delete session</TooltipContent>
              </Tooltip>
            </div>
          </TableCell>
        )
    }
  }

  return (
    <>
      <TableRow
        className={`group border-b border-[hsl(var(--border))] transition-colors duration-150 hover:bg-[hsl(var(--accent))] cursor-pointer ${rowOpacity} ${animateClass}`}
        onClick={() => openSession(isRunning ? 'attach' : 'logs')}
      >
        {columns.map((column) => renderCell(column.key))}
      </TableRow>

      {/* Confirm dialog */}
      <SessionActionConfirmDialog
        action={pendingAction}
        sessionId={session.id}
        onConfirm={(action) => {
          if (action === 'stop') onStop(session)
          else onKill(session)
        }}
        onClose={() => setPendingAction(null)}
      />
    </>
  )
})
