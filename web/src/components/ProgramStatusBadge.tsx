import type { ProgramStatus } from '@/api/types'
import { Badge, type BadgeProps } from '@/components/ui/badge'

const labels = {
  idle: 'Idle',
  working: 'Working',
  done: 'Task Done',
  blocked: 'Input Required',
  error: 'Task Error',
}
const blockedLabels = {
  permission: 'Permission Required',
  question: 'Answer Required',
  auth: 'Authentication Required',
}
const variants: Record<ProgramStatus['state'], BadgeProps['variant']> = {
  idle: 'program-idle',
  working: 'program-working',
  done: 'program-done',
  blocked: 'program-blocked',
  error: 'program-error',
}

export default function ProgramStatusBadge({ report }: { report: ProgramStatus }) {
  const label =
    report.state === 'blocked' && report.kind ? blockedLabels[report.kind] : labels[report.state]
  const progress =
    report.progress != null && (report.state === 'working' || report.state === 'blocked')
      ? ` ${report.progress}%`
      : ''
  const detail = [report.app, report.id, report.title, report.msg].filter(Boolean).join(' · ')
  return (
    <Badge
      variant={variants[report.state]}
      className="max-w-full rounded-md px-2 whitespace-normal text-xs font-medium leading-4 tabular-nums"
      title={`Agent-reported (OSC 7501)${detail ? `: ${detail}` : ''}`}
    >
      {label}
      {progress}
    </Badge>
  )
}
