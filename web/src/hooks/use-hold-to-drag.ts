import { useEffect, useRef, useState, type CSSProperties, type RefObject } from 'react'

import {
  clampPosition,
  loadPosition,
  savePosition,
  type FloatPosition,
} from '@/lib/floating-position'

interface Options {
  /** The press target: the hold-then-drag gesture starts on it. */
  handleRef: RefObject<HTMLElement | null>
  /** The positioned element the gesture moves; its `offsetParent` bounds it. */
  elementRef: RefObject<HTMLElement | null>
  /** localStorage key the dropped position is remembered under. */
  storageKey: string
  /**
   * How long a press must stay still before a move repositions instead of acts.
   * `0` arms with the press itself: for a control whose press has no hold
   * meaning of its own (it only taps and moves), any travelling press is
   * already a reposition.
   */
  armMs?: number
  /** Pointer travel (px) that abandons arming — a moving press is a normal drag. */
  slopPx?: number
  /** A reposition just started: the component's own press gesture is over. */
  onDragStart?: () => void
  /** A reposition just ended. */
  onDragEnd?: () => void
}

interface Controls {
  /** The hold elapsed: the next movement will reposition the element. */
  armed: boolean
  /** The element is following the pointer right now. */
  moving: boolean
  /** Position override for the element; `undefined` means "use the CSS default". */
  style: CSSProperties | undefined
}

/** A press must stay still this long before dragging the button is allowed. */
const ARM_MS = 500
/** Pointer travel (px) before an early move cancels the pending arm. */
const SLOP_PX = 10

interface Gesture {
  originX: number
  originY: number
  /** The element's `offsetLeft/Top` when the press began. */
  startX: number
  startY: number
  container: HTMLElement
  /** The clamped position last written to the element, once moving. */
  applied: FloatPosition | null
}

/**
 * Give a floating button a hold-then-drag gesture: press, keep still past
 * `armMs`, and the button is yours to drag anywhere in its container — the
 * drop is clamped inside the container and remembered in localStorage, so the
 * next visit starts where the last one ended.
 *
 * The still-hold is the contract: movement *before* the arm threshold means
 * the press is the component's own gesture (a tap, the terminal's joystick
 * scroll) and arming is abandoned — the gestures that were there first can
 * never be shadowed by repositioning. The arm and hold timings are
 * independent: a control whose hold opens a menu gets that menu at its own
 * threshold and yields it only once the drag actually starts (`onDragStart`).
 * With `armMs: 0` the contract is dropped instead: the control arms as the
 * press lands, because its press has no other meaning to protect.
 *
 * A press that travelled is a move, and the click the browser sends with that
 * release — the pointer does not have to leave the element for one to arrive —
 * is swallowed, so a reposition can never also activate the button it moved.
 *
 * Listeners live outside React on purpose: the components already own their
 * own pointer handlers, and window-level move/up tracking keeps the drag alive
 * when the pointer outruns the element it is carrying.
 */
export function useHoldToDrag({
  handleRef,
  elementRef,
  storageKey,
  armMs = ARM_MS,
  slopPx = SLOP_PX,
  onDragStart,
  onDragEnd,
}: Options): Controls {
  const [position, setPosition] = useState<FloatPosition | null>(() => loadPosition(storageKey))
  const [armed, setArmed] = useState(false)
  const [moving, setMoving] = useState(false)
  // The gesture listeners read the position outside React's render cycle.
  const positionRef = useRef(position)
  // Arm and drag flags mirrored out of state: the listeners are installed once
  // on mount, but these change on every press.
  const armedRef = useRef(false)
  const movingRef = useRef(false)
  // Held in refs and synced by an effect, so the listeners installed once on
  // mount always call the consumer's latest closure.
  const onDragStartRef = useRef(onDragStart)
  const onDragEndRef = useRef(onDragEnd)
  useEffect(() => {
    onDragStartRef.current = onDragStart
    onDragEndRef.current = onDragEnd
  })

  useEffect(() => {
    const handle = handleRef.current
    const element = elementRef.current
    if (!handle || !element) return

    let armTimer: ReturnType<typeof setTimeout> | null = null
    let gesture: Gesture | null = null

    // A travelling press is a move, so the click the browser sends with that
    // release — it arrives whether or not the pointer left the element — must
    // not reach the consumer as an activation of the button it just moved.
    // Armed as such a press ends, lifted again by the click that follows it, or
    // by a task after the release for one that produces none. React listens on
    // the root, so stopping the event at the handle is what keeps the
    // consumer's own `onClick` from ever seeing it.
    let suppressClick = false
    let releaseSuppression: ReturnType<typeof setTimeout> | null = null
    const clearSuppression = () => {
      suppressClick = false
      if (releaseSuppression) {
        clearTimeout(releaseSuppression)
        releaseSuppression = null
      }
    }
    const onClick = (event: MouseEvent) => {
      if (!suppressClick) return
      // A click dispatched with the release lands before that task, so this is
      // the one the move produced.
      event.stopPropagation()
      clearSuppression()
    }

    const clearArm = () => {
      if (armTimer) {
        clearTimeout(armTimer)
        armTimer = null
      }
    }

    const onPointerMove = (event: PointerEvent) => {
      if (!gesture) return
      const dx = event.clientX - gesture.originX
      const dy = event.clientY - gesture.originY
      const distance = Math.hypot(dx, dy)

      if (!movingRef.current) {
        if (!armedRef.current) {
          // Before the hold elapses, a moving press is the component's own
          // gesture — it is not a prelude to repositioning.
          if (distance > slopPx) clearArm()
          return
        }
        if (distance <= slopPx) return
        movingRef.current = true
        setMoving(true)
        onDragStartRef.current?.()
      }

      const next = clampPosition(
        { x: gesture.startX + dx, y: gesture.startY + dy },
        { width: element.offsetWidth, height: element.offsetHeight },
        { width: gesture.container.clientWidth, height: gesture.container.clientHeight }
      )
      gesture.applied = next
      // Written straight to the element so a state render cannot lag or fight
      // the drag; React only takes over when the drop commits the position.
      element.style.left = `${next.x}px`
      element.style.top = `${next.y}px`
      // Neutralise the default corner anchoring: left+right with an auto
      // width would stretch the element between them.
      element.style.right = 'auto'
      element.style.bottom = 'auto'
    }

    const endGesture = () => {
      if (!gesture) return
      const applied = movingRef.current ? gesture.applied : null
      gesture = null
      clearArm()
      window.removeEventListener('pointermove', onPointerMove)
      window.removeEventListener('pointerup', endGesture)
      window.removeEventListener('pointercancel', endGesture)
      window.removeEventListener('blur', endGesture)
      armedRef.current = false
      setArmed(false)
      if (!movingRef.current) return
      movingRef.current = false
      suppressClick = true
      if (!releaseSuppression) releaseSuppression = setTimeout(clearSuppression, 0)
      setMoving(false)
      onDragEndRef.current?.()
      if (applied) {
        positionRef.current = applied
        setPosition(applied)
        savePosition(storageKey, applied)
      }
    }

    const onPointerDown = (event: PointerEvent) => {
      // A second finger, or a non-primary button, is not a hold.
      if (!event.isPrimary || event.button !== 0) return
      if (gesture) return
      const container = element.offsetParent
      if (!(container instanceof HTMLElement)) return
      gesture = {
        originX: event.clientX,
        originY: event.clientY,
        startX: element.offsetLeft,
        startY: element.offsetTop,
        container,
        applied: null,
      }
      window.addEventListener('pointermove', onPointerMove)
      window.addEventListener('pointerup', endGesture)
      window.addEventListener('pointercancel', endGesture)
      // A lost window would otherwise leave the element stuck mid-drag.
      window.addEventListener('blur', endGesture)
      if (armMs > 0) {
        armTimer = setTimeout(() => {
          armTimer = null
          if (!gesture) return
          armedRef.current = true
          setArmed(true)
        }, armMs)
      } else {
        // No hold to wait out: the press arrives armed, so the next move past
        // the slop is already a reposition.
        armedRef.current = true
        setArmed(true)
      }
    }

    handle.addEventListener('pointerdown', onPointerDown)
    handle.addEventListener('click', onClick)
    return () => {
      handle.removeEventListener('pointerdown', onPointerDown)
      handle.removeEventListener('click', onClick)
      clearArm()
      clearSuppression()
      window.removeEventListener('pointermove', onPointerMove)
      window.removeEventListener('pointerup', endGesture)
      window.removeEventListener('pointercancel', endGesture)
      window.removeEventListener('blur', endGesture)
      gesture = null
      armedRef.current = false
      movingRef.current = false
    }
  }, [armMs, elementRef, handleRef, slopPx, storageKey])

  // Keep a remembered position inside the container as it resizes (window
  // resize, rotation), so the button can never be parked off-screen.
  useEffect(() => {
    const element = elementRef.current
    const container = element?.offsetParent
    if (!element || !(container instanceof HTMLElement)) return
    const clampToContainer = () => {
      const stored = positionRef.current
      if (!stored) return
      const next = clampPosition(
        stored,
        { width: element.offsetWidth, height: element.offsetHeight },
        { width: container.clientWidth, height: container.clientHeight }
      )
      if (next.x === stored.x && next.y === stored.y) return
      positionRef.current = next
      setPosition(next)
    }
    const observer = new ResizeObserver(clampToContainer)
    observer.observe(container)
    return () => observer.disconnect()
  }, [elementRef])

  const style: CSSProperties | undefined = position
    ? { left: position.x, top: position.y, right: 'auto', bottom: 'auto' }
    : undefined
  return { armed, moving, style }
}
