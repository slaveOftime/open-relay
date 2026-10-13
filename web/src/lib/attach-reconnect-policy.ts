/**
 * The attach reconnect policy.
 *
 * Split out of the socket effect because it is arithmetic, and arithmetic with
 * an off-by-one is exactly the kind of bug that survives a visual check. The
 * curve below is what the daemon sees when a phone wakes from background: a
 * 120ms first retry (the common case is a socket iOS killed, not a dead
 * session), doubling to ~2s and staying there.
 */

/** Cap on the backoff: past this, retrying harder does not help anyone. */
export const RECONNECT_MAX_DELAY_MS = 2000

/** First retry delay. Short on purpose — see the module comment. */
export const RECONNECT_BASE_DELAY_MS = 120

/** 2^6 × base is past the cap, so the exponent stops mattering here. */
const MAX_EXPONENT = 6

/**
 * Backoff for the nth reconnect attempt. Exponents are clamped so a socket
 * that has been retried for an hour does not compute an absurd power.
 */
export function reconnectDelayMs(attempt: number): number {
  if (attempt < 1) return 0
  const exponent = Math.min(MAX_EXPONENT, attempt - 1)
  return Math.min(RECONNECT_MAX_DELAY_MS, RECONNECT_BASE_DELAY_MS * 2 ** exponent)
}

/**
 * Close codes that mean "the connection dropped" rather than "the server
 * refused this session". Anything else is still retried — a session that was
 * resized away by another client is as likely as one that was deleted — but the
 * distinction is worth a trace line.
 */
const TRANSIENT_CLOSE_CODES = new Set([1006, 1001, 1005, 0])

export function isTransientClose(code: number): boolean {
  return TRANSIENT_CLOSE_CODES.has(code)
}

/**
 * A reconnect cannot do anything useful while the page is hidden or offline, so
 * it is deferred to the next foreground/online event instead of burning a timer.
 */
export function shouldDeferReconnect(input: { hidden: boolean; offline: boolean }): boolean {
  return input.hidden || input.offline
}

/** What to show the user for a deferred reconnect. */
export function deferredReconnectReason(input: { hidden: boolean; offline: boolean }): string {
  const parts: string[] = []
  if (input.hidden) parts.push('hidden')
  if (input.offline) parts.push('offline')
  return parts.join('+')
}
