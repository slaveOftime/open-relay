import { useEffect, useState } from 'react'
import type { ButtonHTMLAttributes } from 'react'

import { FOLLOW_UP_CLICK_WINDOW_MS } from './repeat-controller'

type PressHandlers = Pick<
  ButtonHTMLAttributes<HTMLButtonElement>,
  | 'onPointerDown'
  | 'onPointerMove'
  | 'onPointerUp'
  | 'onPointerCancel'
  | 'onPointerLeave'
  | 'onContextMenu'
>

export interface TapOptions {
  /** Fires when a press releases still — except as the second tap of a pair. */
  onPress?: () => void
  /** Fires on the second tap of a pair released within `doubleMs`. */
  onDoubleTap?: () => void
  /** Pointer travel (px) that disqualifies a press from being a tap. */
  slopPx?: number
  /** How soon the second tap must land after the first to complete a pair. */
  doubleMs?: number
}

/** How soon the second tap must land after the first to count as a double. */
export const DOUBLE_TAP_MS = 300
/** Pointer travel (px) that cancels a pending tap. */
export const SLOP_PX = 10

/** What one completed press turned out to be. */
export type TapKind = 'single' | 'double'

/**
 * Classifies button presses into taps and double taps — pure, so the contract
 * is testable without a renderer.
 *
 * Activation happens on pointerup rather than in click, so the click the
 * browser synthesizes after a gesture can never double-fire: the consumer's
 * `onClick` checks `isFollowUpClick` and lets only click-without-a-gesture
 * through. Same contract `RepeatController.isFollowUpClick` already gives the
 * quick keys: touch-delivered clicks are not reliably distinguishable from
 * keyboard activations by `detail` (Chrome reports 1, older WebKit 0), so the
 * click's timing against the last gesture event is what tells them apart.
 * Assistive tech and `element.click()` send a click with no gesture at all,
 * or with a `timeStamp` of 0, and stay first activations.
 */
export class TapGestures {
  /**
   * The consumer's callbacks. Held in a plain box that `useTapGestures` keeps
   * current, so the gesture state never needs recreating.
   */
  private callbacks: Pick<TapOptions, 'onPress' | 'onDoubleTap'> = {}

  /** Point the gestures at the consumer's latest callbacks. */
  setCallbacks(callbacks: Pick<TapOptions, 'onPress' | 'onDoubleTap'>): void {
    this.callbacks = callbacks
  }

  private origin: { x: number; y: number } | null = null
  // `timeStamp` of the last gesture event, which is what a follow-up click is
  // measured against. Anchored to the release, never to the press alone.
  private lastGestureAt = Number.NEGATIVE_INFINITY
  // `timeStamp` of the last tap that fired as a single; the next tap within
  // `doubleMs` completes the pair. A travelling press breaks a pending pair —
  // it belongs to whatever the move started (a drag), not to the taps that
  // happened around it.
  private lastTapAt = Number.NEGATIVE_INFINITY

  private readonly slopPx: number
  private readonly doubleMs: number

  constructor(slopPx: number = SLOP_PX, doubleMs: number = DOUBLE_TAP_MS) {
    this.slopPx = slopPx
    this.doubleMs = doubleMs
  }

  readonly handlers: Required<PressHandlers> = {
    onPointerDown: (event) => {
      // A second finger, or a non-primary button, is not a press.
      if (!event.isPrimary || event.button !== 0) return
      // Suppress the press's default. Left alone, the browser focuses the
      // button as the gesture's default action — which lands after a tap has
      // already focused the consumer's own input, blurring it and closing
      // whatever that just opened.
      event.preventDefault()
      this.origin = { x: event.clientX, y: event.clientY }
    },
    onPointerMove: (event) => {
      const origin = this.origin
      if (!origin) return
      const dx = event.clientX - origin.x
      const dy = event.clientY - origin.y
      if (dx * dx + dy * dy > this.slopPx * this.slopPx) {
        this.origin = null
        this.lastTapAt = Number.NEGATIVE_INFINITY
      }
    },
    onPointerUp: (event) => {
      // The release anchors the follow-up window, whatever the press became.
      this.lastGestureAt = event.timeStamp
      // A press that moved away, left the button, or was cancelled is not a tap.
      if (!this.origin) return
      this.origin = null
      const at = event.timeStamp
      if (at - this.lastTapAt <= this.doubleMs) {
        // The pair is spent: the next tap starts a new one.
        this.lastTapAt = Number.NEGATIVE_INFINITY
        this.callbacks.onDoubleTap?.()
        return
      }
      this.lastTapAt = at
      this.callbacks.onPress?.()
    },
    onPointerCancel: (event) => {
      this.lastGestureAt = event.timeStamp
      this.origin = null
    },
    onPointerLeave: () => {
      this.origin = null
    },
    // Long-pressing a touch target raises a context menu on some platforms
    // instead of a click; a floating control never wants one.
    onContextMenu: (event) => {
      event.preventDefault()
    },
  }

  readonly isFollowUpClick = (at: number): boolean => {
    const last = this.lastGestureAt
    return at >= last && at - last <= FOLLOW_UP_CLICK_WINDOW_MS
  }
}

/**
 * Owns one TapGestures for the caller's lifetime and keeps its callbacks
 * current, so the returned handlers stay referentially stable across renders.
 * `slopPx` and `doubleMs` are read once, at creation — they are constants at
 * every call site.
 */
export function useTapGestures(options: TapOptions): TapGestures {
  const [gestures] = useState(() => new TapGestures(options.slopPx, options.doubleMs))
  useEffect(() => {
    gestures.setCallbacks(options)
  })
  return gestures
}
