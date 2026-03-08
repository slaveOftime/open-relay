import type { SessionStatus } from '@/api/types'
import { Badge, type BadgeProps } from '@/components/ui/badge'
import { statusLabel } from '@/utils/format'

interface Props {
  status: SessionStatus
  inputNeeded: boolean
  showDot?: boolean
}

type BadgeVariant = BadgeProps['variant']

function statusVariant(status: SessionStatus, inputNeeded: boolean): BadgeVariant {
  if (inputNeeded) return 'input-needed'
  switch (status) {
    case 'running':
      return 'running'
    case 'stopping':
      return 'stopping'
    case 'failed':
      return 'failed'
    case 'created':
      return 'created'
    default:
      return 'stopped'
  }
}

function dotColor(status: SessionStatus): string {
  switch (status) {
    case 'running':
      return 'bg-green-400'
    case 'stopping':
      return 'bg-yellow-400'
    case 'failed':
      return 'bg-red-400'
    default:
      return 'bg-gray-500'
  }
}

export default function StatusBadge({ status, inputNeeded, showDot = true }: Props) {
  return (
    <Badge
      variant={statusVariant(status, inputNeeded)}
      className="whitespace-nowrap text-xs font-light"
    >
      {showDot && (
        <span
          className={`w-1.5 h-1.5 rounded-full ${dotColor(status)} ${
            status === 'running' ? 'animate-pulse' : ''
          }`}
        />
      )}
      {statusLabel(status, inputNeeded)}
    </Badge>
  )
}
