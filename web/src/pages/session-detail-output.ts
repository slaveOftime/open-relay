/**
 * Pure helpers for the session detail page: is the session live, what does a
 * snapshot need before xterm can render it, and did the visible output move.
 *
 * Kept out of the page file so both can be tested without mounting it.
 */

import type { SessionSummary } from '@/api/types'

/** A session is "live" from the terminal's perspective in these three states. */
export function isSessionRunning(session: SessionSummary | null): boolean {
  return session
    ? session.status === 'running' || session.status === 'stopping' || session.status === 'created'
    : false
}

/**
 * xterm treats a bare LF as "move down, same column", while a PTY expects
 * CRLF. Snapshots record raw PTY bytes, so any `\n` that is not already
 * preceded by `\r` needs one inserted before the terminal sees it.
 */
export function normalizeSnapshotOutputForXterm(output: Uint8Array): Uint8Array {
  let extra = 0
  for (let i = 0; i < output.length; i += 1) {
    if (output[i] === 0x0a && (i === 0 || output[i - 1] !== 0x0d)) {
      extra += 1
    }
  }
  if (extra === 0) return output

  const normalized = new Uint8Array(output.length + extra)
  let writeIndex = 0
  for (let i = 0; i < output.length; i += 1) {
    const byte = output[i]
    if (byte === 0x0a && (i === 0 || output[i - 1] !== 0x0d)) {
      normalized[writeIndex] = 0x0d
      writeIndex += 1
    }
    normalized[writeIndex] = byte
    writeIndex += 1
  }
  return normalized
}

/**
 * Did anything the user can see happen between these two summaries? Used to
 * decide whether the terminal needs a repaint after a summary changes.
 */
export function didSessionVisibleOutputAdvance(
  previous: SessionSummary | null,
  next: SessionSummary
): boolean {
  if (!previous) {
    return Boolean(next.last_output_epoch) || next.last_total_bytes > 0
  }
  if (next.last_output_epoch && next.last_output_epoch !== previous.last_output_epoch) {
    return true
  }
  return next.last_total_bytes > previous.last_total_bytes
}
