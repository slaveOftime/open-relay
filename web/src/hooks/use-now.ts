/**
 * The app's shared wall clock, for relative timestamps that have to stay true
 * while you look at them ("10s ago" is a lie a minute later).
 *
 * One interval serves every subscriber, started with the first and stopped with
 * the last: the alternative — a timer per card — is a list of idle sessions each
 * waking the main thread once a second. The reading is cached rather than read
 * from `Date.now()` per call, because `useSyncExternalStore` compares snapshots
 * by identity and a value that changed on every read would re-render forever.
 */

import { useSyncExternalStore } from 'react'

/** One second: a second is the finest label the clock renders. */
export const CLOCK_TICK_MS = 1000

let now = Date.now()
let timer: ReturnType<typeof setInterval> | null = null
const listeners = new Set<() => void>()

function tick(): void {
  now = Date.now()
  for (const listener of listeners) listener()
}

/** Subscribe to the tick. The interval lives exactly as long as it has consumers. */
export function subscribeClock(listener: () => void): () => void {
  listeners.add(listener)
  if (listeners.size === 1) {
    // First consumer: catch up on however long the reading sat unread, then
    // tick. `useSyncExternalStore` re-reads the snapshot after subscribing, so
    // this first refresh reaches the render that caused the subscription.
    now = Date.now()
    timer = setInterval(tick, CLOCK_TICK_MS)
  }
  return () => {
    listeners.delete(listener)
    if (listeners.size > 0 || timer === null) return
    clearInterval(timer)
    timer = null
  }
}

/** The last tick's `Date.now()`, stable between ticks. */
export function getClockNow(): number {
  return now
}

/** The current time, re-rendering the calling component on every tick. */
export function useNow(): number {
  return useSyncExternalStore(subscribeClock, getClockNow)
}
