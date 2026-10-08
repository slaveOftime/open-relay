import { useCallback, useEffect, useRef } from 'react'
import type { ButtonHTMLAttributes } from 'react'

const REPEAT_DELAY_MS = 400
const REPEAT_INTERVAL_MS = 100

/**
 * Start an action immediately, then again after a short hold delay, then on a
 * fixed interval. `stopRepeat` clears whichever timer is currently pending.
 */
export function useRepeatWhilePressed() {
  const timeoutRef = useRef<number | null>(null)
  const intervalRef = useRef<number | null>(null)
  const actionRef = useRef<(() => void) | null>(null)

  const stopRepeat = useCallback(() => {
    if (timeoutRef.current !== null) {
      window.clearTimeout(timeoutRef.current)
      timeoutRef.current = null
    }
    if (intervalRef.current !== null) {
      window.clearInterval(intervalRef.current)
      intervalRef.current = null
    }
    actionRef.current = null
  }, [])

  const startRepeat = useCallback(
    (action: () => void) => {
      stopRepeat()
      actionRef.current = action
      action()
      timeoutRef.current = window.setTimeout(() => {
        actionRef.current?.()
        intervalRef.current = window.setInterval(() => {
          actionRef.current?.()
        }, REPEAT_INTERVAL_MS)
      }, REPEAT_DELAY_MS)
    },
    [stopRepeat]
  )

  useEffect(() => {
    const stop = () => stopRepeat()
    window.addEventListener('pointerup', stop)
    window.addEventListener('pointercancel', stop)
    window.addEventListener('blur', stop)
    window.addEventListener('keyup', stop)
    return () => {
      window.removeEventListener('pointerup', stop)
      window.removeEventListener('pointercancel', stop)
      window.removeEventListener('blur', stop)
      window.removeEventListener('keyup', stop)
      stopRepeat()
    }
  }, [stopRepeat])

  return { startRepeat, stopRepeat }
}

/** Shared button handlers for hold-to-repeat keys. */
export function holdRepeatProps(
  startRepeat: (action: () => void) => void,
  stopRepeat: () => void,
  action: () => void
): ButtonHTMLAttributes<HTMLButtonElement> {
  return {
    onPointerDown: (event) => {
      if (event.button !== 0) return
      event.preventDefault()
      startRepeat(action)
    },
    onPointerUp: stopRepeat,
    onPointerLeave: stopRepeat,
    onPointerCancel: stopRepeat,
    onContextMenu: (event) => event.preventDefault(),
    onKeyDown: (event) => {
      if (event.key !== 'Enter' && event.key !== ' ') return
      event.preventDefault()
      if (!event.repeat) startRepeat(action)
    },
    onKeyUp: stopRepeat,
    onBlur: stopRepeat,
  }
}
