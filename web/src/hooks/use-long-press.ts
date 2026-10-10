import { useCallback, useEffect, useRef } from 'react'
import type {
  ButtonHTMLAttributes,
  MouseEvent as ReactMouseEvent,
  PointerEvent as ReactPointerEvent,
} from 'react'

import { FOLLOW_UP_CLICK_WINDOW_MS } from './repeat-controller'

type HoldHandlers = Pick<
  ButtonHTMLAttributes<HTMLButtonElement>,
  | 'onPointerDown'
  | 'onPointerMove'
  | 'onPointerUp'
  | 'onPointerCancel'
  | 'onPointerLeave'
  | 'onContextMenu'
>

interface Options {
  /** Fires once, when a press is held past `holdMs`. */
  onLongPress: () => void
  /** Fires when a press is released before `holdMs`. */
  onPress?: () => void
  holdMs?: number
  slopPx?: number
}

export interface LongPressControls {
  /** Props to spread on the trigger button. */
  handlers: HoldHandlers
  /**
   * True when a click at `at` belongs to a tap or hold this hook already ran.
   *
   * Touch-delivered clicks are not reliably distinguishable from keyboard
   * activations by `detail` (Chrome reports 1, older WebKit 0), so the click's
   * timing against the last gesture event is what tells them apart. Assistive
   * tech and `element.click()` send a click with no gesture at all, or with a
   * `timeStamp` of 0, and stay first activations. Same contract
   * `RepeatController.isFollowUpClick` already gives the quick keys.
   */
  isFollowUpClick: (at: number) => boolean
}

/** How long a press is held before it counts as a hold. */
const HOLD_MS = 400
/** Pointer travel (px) that cancels a pending hold. */
const SLOP_PX = 10

/**
 * Press-and-hold gesture for a button: a tap and a held press are two
 * different actions.
 *
 * Activation is handled on pointerup rather than in click, so the click the
 * browser synthesizes after a held press (or the context menu some platforms
 * fire instead of it) can never double-fire — the consumer's `onClick` checks
 * `isFollowUpClick` and lets only click-without-a-gesture through.
 */
export function useLongPress({
  onLongPress,
  onPress,
  holdMs = HOLD_MS,
  slopPx = SLOP_PX,
}: Options): LongPressControls {
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const originRef = useRef<{ x: number; y: number } | null>(null)
  // `timeStamp` of the last gesture event, which is what a follow-up click is
  // measured against. Anchored to the release, never to the press alone: a
  // hold that ran for seconds releases its click just as late as a tap.
  const lastGestureAt = useRef(Number.NEGATIVE_INFINITY)
  // Held in refs and synced by an effect, so the returned handlers stay
  // referentially stable and always call the consumer's latest closure.
  const onLongPressRef = useRef(onLongPress)
  const onPressRef = useRef(onPress)
  useEffect(() => {
    onLongPressRef.current = onLongPress
    onPressRef.current = onPress
  })

  const cancelPending = useCallback(() => {
    if (timerRef.current) {
      clearTimeout(timerRef.current)
      timerRef.current = null
    }
    originRef.current = null
  }, [])

  const onPointerDown = useCallback(
    (event: ReactPointerEvent<HTMLButtonElement>) => {
      // A second finger, or a non-primary button, is not a press-and-hold.
      if (!event.isPrimary || event.button !== 0) return
      // Suppress the press's default. Left alone, the browser focuses the
      // button as the gesture's default action — which lands after a tap has
      // already focused the consumer's own input, blurring it and closing
      // whatever that just opened. The same reason holdRepeatProps parks
      // focus on the button instead of leaving it be.
      event.preventDefault()
      originRef.current = { x: event.clientX, y: event.clientY }
      timerRef.current = setTimeout(() => {
        timerRef.current = null
        originRef.current = null
        lastGestureAt.current = performance.now()
        onLongPressRef.current()
      }, holdMs)
    },
    [holdMs]
  )

  const onPointerMove = useCallback(
    (event: ReactPointerEvent<HTMLButtonElement>) => {
      const origin = originRef.current
      if (!origin) return
      const dx = event.clientX - origin.x
      const dy = event.clientY - origin.y
      if (dx * dx + dy * dy > slopPx * slopPx) cancelPending()
    },
    [cancelPending, slopPx]
  )

  const onPointerUp = useCallback(
    (event: ReactPointerEvent<HTMLButtonElement>) => {
      // The release anchors the follow-up window, for a tap and for a hold
      // that fired mid-press alike.
      lastGestureAt.current = event.timeStamp
      if (!timerRef.current) return
      // Released before the hold elapsed: a tap.
      cancelPending()
      onPressRef.current?.()
    },
    [cancelPending]
  )

  const onPointerCancel = useCallback(
    (event: ReactPointerEvent<HTMLButtonElement>) => {
      lastGestureAt.current = event.timeStamp
      cancelPending()
    },
    [cancelPending]
  )

  const onPointerLeave = useCallback(() => cancelPending(), [cancelPending])
  // A long press raises a context menu on some platforms instead of a click.
  const onContextMenu = useCallback((event: ReactMouseEvent<HTMLButtonElement>) => {
    event.preventDefault()
  }, [])

  const isFollowUpClick = useCallback((at: number) => {
    const last = lastGestureAt.current
    return at >= last && at - last <= FOLLOW_UP_CLICK_WINDOW_MS
  }, [])

  return {
    handlers: {
      onPointerDown,
      onPointerMove,
      onPointerUp,
      onPointerCancel,
      onPointerLeave,
      onContextMenu,
    },
    isFollowUpClick,
  }
}
