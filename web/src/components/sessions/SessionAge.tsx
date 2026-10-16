/**
 * A session's age, as the card shows it.
 *
 * A live session counts up — "10s ago", "2m ago" — because the question a
 * running session raises is how long it has been running. A finished one is
 * dated exactly: that is when the interesting thing happened, and it will not
 * change again. Only the counting branch subscribes to the shared clock, so a
 * list of finished sessions runs no timers.
 */

import { memo } from 'react'

import type { SessionSummary } from '@/api/types'
import { useNow } from '@/hooks/use-now'
import { formatAge, formatTimestamp } from '@/utils/format'
import { isTerminalStatus } from '@/utils/session-status'

/** Tabular digits: the count must not jitter the row as it ticks. */
const CLASS = 'text-xs text-[hsl(var(--muted-foreground))] tabular-nums'

export const SessionAge = memo(function SessionAge({ session }: { session: SessionSummary }) {
  if (isTerminalStatus(session.status)) {
    return <span className={CLASS}>{formatTimestamp(session.created_at)}</span>
  }
  return <LiveAge createdAt={session.created_at} />
})

/** Anything still going: re-rendered on every tick of the shared clock. */
function LiveAge({ createdAt }: { createdAt: string }) {
  const now = useNow()
  return <span className={CLASS}>{formatAge(createdAt, now)}</span>
}
