/**
 * The tag pills shown on a session row or card.
 *
 * Tags are normalized first (see `normalizeSessionTags`) so a row never
 * renders a duplicate or an empty pill, and long tags stay truncated
 * instead of stretching the table.
 */

import type { SessionSummary } from '@/api/types'
import { Badge } from '@/components/ui/badge'
import { normalizeSessionTags } from '@/utils/sessionTags'

export function SessionTagList({
  tags,
  className = '',
  emptyLabel = null,
}: {
  tags: SessionSummary['tags']
  className?: string
  emptyLabel?: string | null
}) {
  const normalizedTags = normalizeSessionTags(tags)
  if (normalizedTags.length === 0) {
    if (emptyLabel === null) return null
    return <span className="text-xs text-[hsl(var(--muted-foreground))]">{emptyLabel}</span>
  }

  return (
    <div className={`flex min-w-0 max-w-full items-center ${className}`.trim()}>
      {normalizedTags.map((tag) => (
        <Badge key={tag} variant="accent" className="min-w-0 max-w-full text-[10px] font-semibold">
          <span className="min-w-0 truncate">#{tag}</span>
        </Badge>
      ))}
    </div>
  )
}
