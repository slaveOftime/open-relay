/**
 * The attach viewport's idle border.
 *
 * A live terminal shows a "quiet" border after `delay` of no visible output;
 * any activity restarts the countdown, and any user interaction disarms it
 * outright. The delay is deliberately long, because the border only exists to
 * mark a genuinely quiet terminal and flickering it would be noise.
 *
 * The mode/connected/mounted checks are read through refs rather than props so
 * the timer callback never needs to be re-created when they change.
 */

import { useCallback, useEffect, useRef, useState } from 'react'

const ATTACH_IDLE_BORDER_DELAY_MS = 10_000

interface AttachIdleOptions {
  /** Current view mode, read when the timer fires. */
  mode: { current: string | null }
  /** Whether the attach socket is connected. */
  connected: { current: boolean }
  /** Page-mount flag; guards setState during teardown. */
  mounted: { current: boolean }
  delayMs?: number
}

export interface AttachIdleAnimation {
  /** True while the border should be drawn. */
  isIdle: boolean
  /** Output happened: restart the countdown from zero. */
  noteVisibleActivity: () => void
  /** The user touched something: no countdown until the next output. */
  noteUserActivity: () => void
  /** Stop the countdown and draw the border immediately (session needs input). */
  requestIdleBorder: () => void
  /** Stop a pending countdown without changing the border. */
  clearTimer: () => void
  /** Cancel any pending timer on unmount. */
  teardown: () => void
}

export function useAttachIdleAnimation({
  mode,
  connected,
  mounted,
  delayMs = ATTACH_IDLE_BORDER_DELAY_MS,
}: AttachIdleOptions): AttachIdleAnimation {
  const [isIdle, setIsIdle] = useState(false)
  const timerRef = useRef<number | null>(null)
  const armedRef = useRef(false)

  const clearTimer = useCallback(() => {
    if (timerRef.current !== null) {
      window.clearTimeout(timerRef.current)
      timerRef.current = null
    }
  }, [])

  const setIdle = useCallback(
    (next: boolean) => {
      if (mounted.current) setIsIdle(next)
    },
    [mounted]
  )

  const stop = useCallback(() => {
    clearTimer()
    setIdle(false)
  }, [clearTimer, setIdle])

  const disarm = useCallback(() => {
    armedRef.current = false
    stop()
  }, [stop])

  const schedule = useCallback(() => {
    clearTimer()
    if (!armedRef.current || mode.current !== 'attach' || !connected.current) {
      setIdle(false)
      return
    }

    timerRef.current = window.setTimeout(() => {
      timerRef.current = null
      if (!mounted.current || mode.current !== 'attach' || !connected.current) return
      if (!armedRef.current) return
      setIdle(true)
    }, delayMs)
  }, [clearTimer, connected, delayMs, mode, mounted, setIdle])

  const noteVisibleActivity = useCallback(() => {
    armedRef.current = true
    setIsIdle(false)
    schedule()
  }, [schedule, setIdle])

  const noteUserActivity = useCallback(() => {
    disarm()
  }, [disarm])

  /** For the "input needed" branch: no countdown, border drawn now. */
  const requestIdleBorder = useCallback(() => {
    armedRef.current = false
    clearTimer()
    setIdle(true)
  }, [clearTimer, setIdle])

  // A timer fired or a mode change can leave one pending past teardown.
  useEffect(() => clearTimer, [clearTimer])

  return {
    isIdle,
    noteVisibleActivity: noteVisibleActivity,
    noteUserActivity,
    requestIdleBorder,
    clearTimer,
    teardown: clearTimer,
  }
}
