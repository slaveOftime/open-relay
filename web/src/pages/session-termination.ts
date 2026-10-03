import type { SessionStatus, SessionSummary } from '@/api/types'

/** The two terminate actions reachable from a confirm dialog. */
export type SessionTermination = 'stop' | 'kill'

/**
 * Status the client claims the moment a terminate request is confirmed, until
 * the daemon's own `session_updated` replaces it.
 *
 * The daemon treats `stopping` as internal bookkeeping: it moves the runtime to
 * that state before it starts draining, but `stop_session`/`kill_session` only
 * publish a summary (and only answer the HTTP request) once the whole drain has
 * finished — up to `limits.stop_grace_seconds`, 5s by default. A view that only
 * renders authoritative summaries therefore keeps showing `running` for the
 * entire grace window, so the pending state has to be claimed client-side.
 */
export const PENDING_TERMINATION_STATUS: SessionStatus = 'stopping'

/** True once the session is already stopping or finished. */
export function isTerminationSettled(status: SessionStatus): boolean {
  return (
    status === 'stopping' || status === 'stopped' || status === 'killed' || status === 'failed'
  )
}

/**
 * The summary to render while a terminate request is in flight, or `null` when
 * the session is already stopping/finished.
 *
 * Returning `null` for settled sessions is what keeps the optimistic write
 * idempotent: a click that races the daemon (or a second click) must never
 * downgrade a finished session back to `stopping`, and re-sending the request
 * would make the daemon restart its escalation schedule and push the kill
 * deadline further out.
 */
export function withPendingTermination(session: SessionSummary): SessionSummary | null {
  if (isTerminationSettled(session.status)) return null
  return { ...session, status: PENDING_TERMINATION_STATUS }
}

/** Replaces the status of one session in a loaded page of summaries. */
export function withSessionStatus(
  items: SessionSummary[],
  sessionId: string,
  status: SessionStatus
): SessionSummary[] {
  const index = items.findIndex((item) => item.id === sessionId)
  if (index === -1 || items[index].status === status) return items
  const next = items.slice()
  next[index] = { ...next[index], status }
  return next
}

/**
 * Undoes an optimistic status write after the request failed.
 *
 * Only applies while the row still carries `pendingStatus`: a `session_updated`
 * that landed in the meantime is authoritative and must not be rolled back to a
 * status we merely guessed at.
 */
export function revertSessionStatus(
  items: SessionSummary[],
  sessionId: string,
  pendingStatus: SessionStatus,
  previousStatus: SessionStatus
): SessionSummary[] {
  const index = items.findIndex((item) => item.id === sessionId)
  if (index === -1 || items[index].status !== pendingStatus) return items
  return withSessionStatus(items, sessionId, previousStatus)
}