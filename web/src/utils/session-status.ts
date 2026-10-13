/**
 * Where "is this session finished" is decided, for rows, cards and group
 * headers. A finished session dims and loses its live affordances; anything
 * in `running`/`stopping`/`created` keeps them.
 */

import type { SessionSummary } from '@/api/types'

export function isTerminalStatus(status: SessionSummary['status']): boolean {
  return status === 'stopped' || status === 'killed' || status === 'failed'
}
