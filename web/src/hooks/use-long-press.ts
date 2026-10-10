import { useCallback, useEffect, useRef } from 'react'
import type {
  ButtonHTMLAttributes,
  MouseEvent as ReactMouseEvent,
  PointerEvent as ReactPointerEvent,
} from 'react'

type HoldHandlers = Pick<
  ButtonHTMLAttributes<HTMLButtonElement>,
  | 'onPointerDown'
  | 'onPointerMove'
  | 'onPointerUp'
  | 'onPointerCancel'
  | 'onPointerLeave'
  | 'onContextMenu'
>

/** How long a press is held before it counts as a hold. */
const HOLD_MS = 400
/** Pointer travel (px) that cancels a pending hold — a scroll or drag, not a press. */
const SLOP_PX = 10

interface Options {
  /** Fires once, when a press is held past the hold duration. */
  onLongPress: () => void
  /** Fires when a press is released before the hold duration — a tap. */
  onPress?: () => void
  holdMs?: number
  slopPx?: number
}

/**
 * Press-and-hold gesture for a button: a tap and a held press are two
 * different actions.
 *
 * Activation is handled on pointerup rather than in click, so the click the
 * browser synthesizes after a held press — or the context menu some platforms
 * fire on a long press instead of a click — can never double-fire. Keyboard
 * activation still reaches the button as a click with `detail === 0`, which
 * the consumer handles itself.
 */
export function useLongPress({
  onLongPress,
  onPress,
  holdMs = HOLD_MS,
  slopPx = SLOP_PX,
}: Options): HoldHandlers {
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const originRef = useRef<{ x: number; y: number } | null>(null)
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
      originRef.current = { x: event.clientX, y: event.clientY }
      timerRef.current = setTimeout(() => {
        timerRef.current = null
        originRef.current = null
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

  const onPointerUp = useCallback(() => {
    if (!timerRef.current) return
    // Released before the hold elapsed: a tap.
    cancelPending()
    onPressRef.current?.()
  }, [cancelPending])

  const onPointerCancel = useCallback(() => cancelPending(), [cancelPending])
  const onPointerLeave = useCallback(() => cancelPending(), [cancelPending])
  // A long press raises a context menu on some platforms instead of a click.
  const onContextMenu = useCallback((event: ReactMouseEvent<HTMLButtonElement>) => {
    event.preventDefault()
  }, [])

  return {
    onPointerDown,
    onPointerMove,
    onPointerUp,
    onPointerCancel,
    onPointerLeave,
    onContextMenu,
  }
}
