/**
 * The label on a group header in the sessions table.
 *
 * What a group header looks like depends on what it is grouping by: a tag
 * is a badge (or plain text for the untagged bucket), a command gets its
 * logo, and anything else is the text itself.
 */

import type { SessionSummary } from '@/api/types'
import CommandLogo from '@/components/CommandLogo'
import { Badge } from '@/components/ui/badge'
import type { GroupBy } from './group-by'

export function GroupHeaderLabel({
  groupBy,
  keyLabel,
  items,
}: {
  groupBy: GroupBy
  keyLabel: string
  items: SessionSummary[]
}) {
  if (groupBy === 'tag') {
    return keyLabel === '(untagged)' ? (
      <>{keyLabel}</>
    ) : (
      <Badge
        variant="outline"
        className="border-[hsl(var(--border))] px-2 py-0 text-[10px] font-medium text-[hsl(var(--muted-foreground))]"
      >
        {keyLabel}
      </Badge>
    )
  }
  if (groupBy !== 'command') return <span className="break-all">{keyLabel}</span>
  const groupCommand = items[0]?.command ?? keyLabel
  return (
    <span className="inline-flex items-center gap-2 wrap-break-word">
      <CommandLogo command={groupCommand} size={24} />
      <span>{keyLabel}</span>
    </span>
  )
}
