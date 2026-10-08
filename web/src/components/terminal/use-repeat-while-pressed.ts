import { useEffect, useMemo, useState } from 'react'
import type { ButtonHTMLAttributes } from 'react'
import { RepeatController, type RepeatSource } from './repeat-controller'

export interface RepeatControls {
  startRepeat: (action: () => void, source: RepeatSource) => void
  /** `at` is the `timeStamp` of the event that is ending the gesture. */
  stopRepeat: (at?: number) => void
  /** True when this pointer ending should end the hold. */
  endsOnPointer: (pointerId: number) => boolean
  /** True when this key release should end the hold. */
  endsOnKey: (key: string) => boolean
  /** True when a click at `event.timeStamp` repeats an activation we handled. */
  isFollowUpClick: (at: number) => boolean
}

/**
 * Owns one RepeatController for the caller's lifetime and forwards the window
 * events that must end a hold even when the pointer or focus moved away.
 */
export function useRepeatWhilePressed(): RepeatControls {
  const [controller] = useState(() => new RepeatController(window))

  useEffect(() => {
    const onPointerEnd = (event: PointerEvent) => {
      if (controller.endsOnPointer(event.pointerId)) controller.stop(event.timeStamp)
    }
    const onKeyUp = (event: KeyboardEvent) => {
      if (controller.endsOnKey(event.key)) controller.stop(event.timeStamp)
    }
    const onBlur = () => controller.stop()
    window.addEventListener('pointerup', onPointerEnd)
    window.addEventListener('pointercancel', onPointerEnd)
    window.addEventListener('keyup', onKeyUp)
    window.addEventListener('blur', onBlur)
    return () => {
      window.removeEventListener('pointerup', onPointerEnd)
      window.removeEventListener('pointercancel', onPointerEnd)
      window.removeEventListener('keyup', onKeyUp)
      window.removeEventListener('blur', onBlur)
    }
  }, [controller])

  // Unmounting the owner mid-hold must not leave a timer firing.
  useEffect(() => () => controller.stop(), [controller])

  return useMemo<RepeatControls>(
    () => ({
      startRepeat: (action, source) => controller.start(action, source),
      stopRepeat: (at) => controller.stop(at),
      endsOnPointer: (pointerId) => controller.endsOnPointer(pointerId),
      endsOnKey: (key) => controller.endsOnKey(key),
      isFollowUpClick: (at) => controller.isFollowUpClick(at),
    }),
    [controller]
  )
}

/**
 * Park DOM focus on a hold-repeat button.
 *
 * Suppressing the press's default is not enough on its own: it leaves focus
 * wherever it was, and in attach mode that is xterm's hidden textarea. The
 * soft keyboard then stays up over the panel, and the tap/keyboard-driven
 * viewport churn drags the page out from under the finger — cancelling the
 * pointer and cutting the repeat short. A `<button>` never raises a keyboard,
 * so focusing it both answers the tap and drops the IME.
 */
function parkFocus(button: HTMLButtonElement): void {
  button.focus({ preventScroll: true })
}

/**
 * Shared button handlers for hold-to-repeat keys.
 *
 * A hold sends on pointerdown/keydown and repeats while it is held; a click
 * only sends when no pointer/key activation preceded it, which is how assistive
 * technology (and `element.click()`) activates a button.
 */
export function holdRepeatProps(
  controls: RepeatControls,
  action: () => void
): ButtonHTMLAttributes<HTMLButtonElement> {
  return {
    onPointerDown: (event) => {
      if (event.button !== 0) return
      event.preventDefault()
      parkFocus(event.currentTarget)
      controls.startRepeat(action, { pointerId: event.pointerId, at: event.timeStamp })
    },
    // Belt and braces over the pointerdown above on browsers where that does
    // not suppress the compatibility mouse events: keeping focus on the button
    // is what stops xterm from reclaiming it on every tap.
    onMouseDown: (event) => {
      event.preventDefault()
      parkFocus(event.currentTarget)
    },
    onKeyDown: (event) => {
      if (event.key !== 'Enter' && event.key !== ' ') return
      event.preventDefault()
      // Auto-repeat keydowns are already covered by the interval.
      if (!event.repeat) controls.startRepeat(action, { key: event.key, at: event.timeStamp })
    },
    onClick: (event) => {
      // The browser's own follow-up click for a tap or key press we already
      // handled: sending again would double the key. Everything else (assistive
      // tech, programmatic activation) is a first activation.
      if (controls.isFollowUpClick(event.timeStamp)) return
      controls.stopRepeat()
      action()
    },
    onPointerUp: (event) => controls.stopRepeat(event.timeStamp),
    onPointerLeave: () => controls.stopRepeat(),
    onPointerCancel: (event) => controls.stopRepeat(event.timeStamp),
    onKeyUp: (event) => controls.stopRepeat(event.timeStamp),
    onBlur: () => controls.stopRepeat(),
    onContextMenu: (event) => event.preventDefault(),
  }
}
