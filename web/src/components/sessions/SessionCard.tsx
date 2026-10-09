/**
 * One session as a mobile card.
 *
 * Same session, different layout: the card stacks id/status/bytes over the
 * command and puts its actions in a footer bar.
 */

import { memo, useState } from 'react'
import { Link, useNavigate } from 'react-router-dom'

import type { SessionSummary } from '@/api/types'
import CommandLogo from '@/components/CommandLogo'
import SessionActivitySparkline from '@/components/SessionActivitySparkline'
import SessionActionConfirmDialog from '@/components/SessionActionConfirmDialog'
import StatusBadge from '@/components/StatusBadge'
import { SessionNotificationButton } from '@/components/sessions/SessionNotificationButton'
import { SessionPinButton } from '@/components/sessions/SessionPinButton'
import { SessionTagList } from '@/components/sessions/SessionTagList'
import { buildSessionHref } from '@/components/sessions/session-href'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardFooter } from '@/components/ui/card'
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'
import { CopyIcon, FileTextIcon, Link2Icon, StopIcon, TrashIcon } from '@radix-ui/react-icons'
import {
  formatByteSize,
  formatTimestamp,
  normalizeCwdPath,
  sessionDisplayName,
} from '@/utils/format'
import { isTerminalStatus } from '@/utils/sessionStatus'

export const SessionCard = memo(function SessionCard({
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
  showCwd,
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
  showCwd?: boolean
}) {
  const navigate = useNavigate()
  const [pendingAction, setPendingAction] = useState<'stop' | 'kill' | null>(null)
  const isRunning =
    session.status === 'running' || session.status === 'stopping' || session.status === 'created'
  const attachHref = buildSessionHref(session.id, 'attach', node)
  const logsHref = buildSessionHref(session.id, 'logs', node)

  const titleTone = isTerminalStatus(session.status)
    ? 'text-[hsl(var(--foreground))]/70'
    : 'text-[hsl(var(--foreground))]'
  const animateClass = animateIn ? 'animate-row-slide-in' : ''
  const opacityClass =
    session.status === 'stopped' || session.status === 'killed' || session.status === 'failed'
      ? 'opacity-60'
      : ''

  const deleteButton = (
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
  )

  function openSession(mode: 'attach' | 'logs') {
    navigate(buildSessionHref(session.id, mode, node))
  }

  return (
    <>
      <Card
        className={`relative rounded-xl shadow-none mx-1 my-2 overflow-hidden flex flex-col transition-colors hover:border-[hsl(var(--border))]/80 ${animateClass} ${opacityClass}`}
      >
        <CardContent className="px-2 pt-2 pb-2 flex flex-col gap-1 relative">
          {/* Row 1: id, status, pid, created at */}
          <div
            data-node-swipe-ignore
            className="z-10 flex items-center gap-2 overflow-x-auto whitespace-nowrap"
          >
            <button
              className="font-mono text-sm text-[hsl(var(--foreground))] font-semibold hover:text-[hsl(var(--primary))] transition-colors"
              onClick={() => onEditSession(session)}
            >
              {session.id.slice(0, 7)}
            </button>
            <span className="text-xs text-[hsl(var(--muted-foreground))] tabular-nums">
              {formatTimestamp(session.created_at)}
            </span>
            <div className="text-[hsl(var(--muted-foreground))] text-xs font-mono tabular-nums">
              {formatByteSize(session.last_total_bytes)}
            </div>
            <div className="flex-1" />
            <StatusBadge status={session.status} inputNeeded={session.input_needed} />
          </div>

          {/* Row 2: command + title */}
          <div className="z-10" onClick={() => openSession(isRunning ? 'attach' : 'logs')}>
            <div className={`flex min-w-0 items-center gap-2 ${titleTone}`}>
              <CommandLogo command={session.command} size={36} />
              <div className="min-w-0 flex-1 line-clamp-5 break-all">
                {session.title?.trim() && (
                  <span className="block leading-4 text-[hsl(var(--primary))]">
                    {session.title.trim()}
                  </span>
                )}
                <span className="block leading-4">{sessionDisplayName(session)}</span>
              </div>
            </div>
          </div>

          {/* Row 3: cwd */}
          {showCwd && session.cwd && (
            <div className="z-10 text-sm leading-4 text-[hsl(var(--muted-foreground))] font-mono break-all">
              {normalizeCwdPath(session.cwd)}
            </div>
          )}

          <div className="z-10 flex flex-wrap items-center gap-2">
            {session.tags.length > 0 && (
              <div className="min-w-0 flex-1">
                <SessionTagList tags={session.tags} className="flex-1 flex-wrap gap-1.5" />
              </div>
            )}
          </div>

          {/* Row 4: activity sparkline */}
          {session.status === 'running' && (
            <div className="pt-1 w-full opacity-20 absolute pointer-events-none z-0 left-0 right-0 -bottom-1">
              <SessionActivitySparkline
                sessionId={session.id}
                node={node}
                isRunning={isRunning}
                fullWidth
                height={60}
                className="w-full"
              />
            </div>
          )}
        </CardContent>

        <div className="border-t border-[hsl(var(--border))]" />

        {/* Action bar */}
        <CardFooter
          className={`flex items-center ${isRunning ? 'flex-row-reverse' : ''} gap-1 px-2 py-1 overflow-x-auto`}
          onClick={(e) => e.stopPropagation()}
        >
          {isRunning && (
            <Button
              asChild
              variant="outline"
              className="border-[hsl(var(--primary))] text-[hsl(var(--primary))]"
              size="sm"
            >
              <Link to={attachHref}>
                <Link2Icon className="h-4 w-4" />
                Attach
              </Link>
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
              {deleteButton}
              <SessionNotificationButton
                enabled={session.notifications_enabled}
                disabled={!isRunning}
                pending={notificationsPending}
                onToggle={() => onToggleNotifications(session)}
              />
              <SessionPinButton pinned={pinned ?? false} onToggle={() => onTogglePin(session)} />
            </>
          )}
          <div className="flex-1"></div>
          {!isRunning && deleteButton}
          <Button asChild variant="ghost" size="icon">
            <Link to={logsHref} aria-label="Logs">
              <FileTextIcon className="h-4 w-4" />
            </Link>
          </Button>
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
        </CardFooter>
      </Card>

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
