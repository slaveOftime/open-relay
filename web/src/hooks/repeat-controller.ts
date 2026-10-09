/**
 * RepeatController — the hold-to-repeat state machine shared by the radial
 * quick keys and AttachPanel's key rows.
 *
 * Extracted from the hook so the timing and gesture-matching rules are a pure,
 * testable state machine instead of a web of refs and timers. It owns:
 *
 * - the immediate send, the delayed follow-up and its interval,
 * - which pointer/keyboard gesture claimed the hold, so an unrelated keyup or
 *   a second finger cannot end someone else's repeat,
 * - the timestamp window that tells the browser's follow-up `click` apart from
 *   a synthetic activation: assistive tech and `element.click()` activate a
 *   button with a bare click event and no pointer/key events at all, so the
 *   pointer path must not be the only way to trigger the action.
 */

/** Delay before the repeat kicks in; mirrors native key-repeat feel. */
export const REPEAT_DELAY_MS = 400
export const REPEAT_INTERVAL_MS = 100

/**
 * How long after a pointer/key-down activation a click is still considered the
 * same gesture. A real click follows its pointerdown within a frame or two;
 * a synthetic click from assistive tech arrives far later (or with a
 * `timeStamp` of 0), which is what keeps that path working.
 */
export const FOLLOW_UP_CLICK_WINDOW_MS = 500

export interface RepeatTimers {
  setTimeout(handler: () => void, ms: number): number
  setInterval(handler: () => void, ms: number): number
  clearTimeout(id: number): void
  clearInterval(id: number): void
}

/** Where a hold came from. `at` is `event.timeStamp` of that gesture. */
export interface RepeatSource {
  pointerId?: number
  key?: string
  at: number
}

export class RepeatController {
  private readonly timers: RepeatTimers
  private action: (() => void) | null = null
  private delayId: number | null = null
  private intervalId: number | null = null
  /** Pointer that claimed the hold; null for a keyboard hold. */
  private pointerId: number | null = null
  /** Key that claimed the hold; null for a pointer hold. */
  private key: string | null = null
  /** `timeStamp` of the last gesture event this controller saw. */
  private lastGestureAt = Number.NEGATIVE_INFINITY

  constructor(timers: RepeatTimers) {
    this.timers = timers
  }

  /** Run `action` now, once more after the delay, then on the interval. */
  start(action: () => void, source: RepeatSource): void {
    this.stop()
    this.action = action
    this.pointerId = source.pointerId ?? null
    this.key = source.key ?? null
    this.lastGestureAt = source.at
    action()
    this.delayId = this.timers.setTimeout(() => {
      this.action?.()
      this.intervalId = this.timers.setInterval(() => this.action?.(), REPEAT_INTERVAL_MS)
    }, REPEAT_DELAY_MS)
  }

  /**
   * Cancel the pending delay/interval. Pass the `timeStamp` of the event that
   * is ending the gesture so the click the browser fires right afterwards is
   * still recognised as that gesture's follow-up.
   */
  stop(at?: number): void {
    if (at !== undefined) this.lastGestureAt = at
    if (this.delayId !== null) {
      this.timers.clearTimeout(this.delayId)
      this.delayId = null
    }
    if (this.intervalId !== null) {
      this.timers.clearInterval(this.intervalId)
      this.intervalId = null
    }
    this.action = null
    this.pointerId = null
    this.key = null
  }

  /** True when this pointer ending should end the current hold. */
  endsOnPointer(pointerId: number): boolean {
    return this.pointerId === null || this.pointerId === pointerId
  }

  /** True when this key release should end the current hold. */
  endsOnKey(key: string): boolean {
    return this.key === null || this.key === key
  }

  /**
   * True when a click at `at` belongs to a hold this controller already ran,
   * so handling it would send the key twice. The window is anchored to the
   * last gesture event (the press or its release), never to the press alone:
   * a hold that ran for seconds releases its click just as late as a tap.
   */
  isFollowUpClick(at: number): boolean {
    return at >= this.lastGestureAt && at - this.lastGestureAt <= FOLLOW_UP_CLICK_WINDOW_MS
  }
}
