import ProgramStatusBadge from './ProgramStatusBadge'
import type { ProgramStatus, SessionStatus } from '@/api/types'
import { Badge, type BadgeProps } from '@/components/ui/badge'
import { statusLabel } from '@/utils/format'

interface Props {
  status: SessionStatus
  inputNeeded: boolean
  showDot?: boolean
  programStatus?: ProgramStatus | null
}

type BadgeVariant = BadgeProps['variant']

function statusVariant(status: SessionStatus, inputNeeded: boolean): BadgeVariant {
  if (inputNeeded) return 'input-needed'
  switch (status) {
    case 'running':
      return 'running'
    case 'stopping':
      return 'stopping'
    case 'killed':
      return 'killed'
    case 'failed':
      return 'failed'
    case 'created':
      return 'created'
    default:
      return 'stopped'
  }
}

function dotColor(status: SessionStatus, inputNeeded: boolean): string {
  if (inputNeeded) return 'bg-amber-400'
  switch (status) {
    case 'running':
      return 'bg-green-400'
    case 'stopping':
      return 'bg-yellow-400'
    case 'killed':
      return 'bg-orange-500'
    case 'failed':
      return 'bg-red-400'
    default:
      return 'bg-gray-500'
  }
}

export default function StatusBadge({ status, inputNeeded, showDot = true, programStatus }: Props) {
  // A program report replaces the lifecycle badge: the report is the single
  // visible status, while the lifecycle remains available from the API.
  if (programStatus) {
    return (
      <span className="inline-flex max-w-full items-center gap-1">
        <ProgramStatusBadge report={programStatus} />
      </span>
    )
  }

  // Without a report the lifecycle is authoritative; a stale flag cannot make
  // an ended process ready for input.
  inputNeeded = status === 'running' && inputNeeded
  const variant = statusVariant(status, inputNeeded)
  const label = statusLabel(status, inputNeeded)
  const shouldPulseDot = status === 'running'

  return (
    <span className="inline-flex max-w-full items-center gap-1">
      <Badge
        key={`${status}:${inputNeeded ? 'input-needed' : 'status'}`}
        variant={variant}
        className="whitespace-nowrap text-xs font-light"
        title={`Process: ${statusLabel(status, false)}`}
      >
        {showDot && (
          <span
            className={`w-1.5 h-1.5 rounded-full ${dotColor(status, inputNeeded)} ${shouldPulseDot ? 'animate-pulse' : ''}`}
          />
        )}
        <span>{label}</span>
      </Badge>
    </span>
  )
}
