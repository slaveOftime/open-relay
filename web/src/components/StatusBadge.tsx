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
  // Lifecycle remains authoritative; retained reports cannot make an ended
  // process ready for input. Show readiness without implying a blocked prompt.
  inputNeeded = status === 'running' && inputNeeded
  const readyForInput =
    inputNeeded && (programStatus?.state === 'idle' || programStatus?.state === 'done')
  const variant = statusVariant(status, inputNeeded)
  const label = readyForInput ? 'Ready for Input' : statusLabel(status, inputNeeded)
  const shouldPulseDot =
    (inputNeeded && !readyForInput) ||
    (status === 'running' && (!programStatus || programStatus.state === 'working'))

  return (
    <span className="inline-flex max-w-full items-center gap-1">
      {programStatus && <ProgramStatusBadge report={programStatus} />}
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
