/**
 * Mobile list gestures: pull-to-refresh and horizontal node swipe.
 *
 * Both are one continuous touch, so they share one set of state: the pending
 * transform offsets, the frame that paints them, and the label. The interesting
 * part is that they are drawn by *writing styles directly* rather than through
 * React state — a gesture runs at frame rate and re-rendering the whole list on
 * every touchmove is how you drop frames on a mid-range phone.
 *
 * `shouldSuppressClick` is the other half: a finger that dragged should not also
 * open the card it started on, so the pointerdown handler asks before it lets
 * the click through.
 */

import { useCallback, useRef } from 'react'

import type { NodeSummary } from '@/api/types'

import {
  nodeAfterSwipe,
  pullDragOffset,
  shouldRefreshOnPull,
  swipeDragOffset,
} from '@/lib/sessions-node-swipe'

/** How far the list slides while a node swipe is in progress. */
const SWIPE_EDGE = 80
/** How far down the pull has to go before it becomes a refresh. */
const PULL_FULL = 48
/** Where the list sits while the refresh is actually running. */
const PULL_HELD_OFFSET = 56
/** How long a finished gesture still swallows clicks, to eat the tap tail. */
const GESTURE_CLICK_SUPPRESS_MS = 350
const SWIPE_MAX_OPACITY = 48
const PULL_OPACITY_SHADE = 0.12

interface GestureState {
  x: number
  y: number
  /** True when the gesture started at the top of a scrollable list. */
  canPull: boolean
}

export interface SessionListGestures {
  /** Attach to the list container. */
  onTouchStart: (event: React.TouchEvent<HTMLDivElement>) => void
  onTouchMove: (event: React.TouchEvent<HTMLDivElement>) => void
  onTouchEnd: (event: React.TouchEvent<HTMLDivElement>) => void
  onTouchCancel: () => void
  /** A pointer that is not a finger cancelled any gesture in progress. */
  noteNonTouchPointer: () => void
  /**
   * True when a click landing right now is the tail of a finished gesture
   * rather than an intent to open a card. Resets the window once it fires, so a
   * later click on the same list is not swallowed too.
   */
  shouldSuppressClick: () => boolean
  /** The element the list slides; the hook writes its transform. */
  contentRef: React.RefObject<HTMLDivElement | null>
  /** The pull indicator; likewise. */
  indicatorRef: React.RefObject<HTMLDivElement | null>
  /** The "Pull to refresh" label; the hook swaps its text. */
  labelRef: React.RefObject<HTMLSpanElement | null>
}

export interface GestureCallbacks {
  selectedNode: string | null
  nodes: readonly NodeSummary[]
  /** True while a load is in flight; blocks a second pull. */
  loading: boolean
  refreshing: boolean
  onNodeChange: (node: string | null) => void
  onRefresh: () => Promise<unknown>
}

export function useSessionListGestures(callbacks: GestureCallbacks): SessionListGestures {
  const { selectedNode, nodes, loading, refreshing, onNodeChange, onRefresh } = callbacks

  const startRef = useRef<GestureState | null>(null)
  const mobileOffsetRef = useRef(0)
  const pullOffsetRef = useRef(0)
  const frameRef = useRef<number | null>(null)
  const pullRefreshingRef = useRef(false)
  const contentRef = useRef<HTMLDivElement | null>(null)
  const indicatorRef = useRef<HTMLDivElement | null>(null)
  const labelRef = useRef<HTMLSpanElement | null>(null)
  const lastSwipeAtRef = useRef(0)

  /** A mouse or pen cancels a gesture the finger left half-finished. */
  const noteNonTouchPointer = useCallback(() => {
    lastSwipeAtRef.current = 0
  }, [])

  const draw = useCallback((animate: boolean) => {
    const content = contentRef.current
    if (!content) return
    const indicator = indicatorRef.current
    const reducedMotion = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches
    const x = reducedMotion ? 0 : mobileOffsetRef.current
    const y = reducedMotion ? 0 : pullOffsetRef.current
    const pulling = pullOffsetRef.current > 0
    const transition =
      animate && !reducedMotion
        ? 'transform 180ms cubic-bezier(0.2, 0.8, 0.2, 1), opacity 180ms ease-out'
        : 'none'
    content.style.transition = transition
    content.style.transform = `translate3d(${x}px, ${y}px, 0)`
    content.style.opacity = String(1 - (Math.abs(x) / SWIPE_MAX_OPACITY) * PULL_OPACITY_SHADE)
    if (indicator) {
      indicator.style.transition = transition
      const indicatorOpacity =
        pullRefreshingRef.current || (reducedMotion && pulling) ? 1 : Math.min(1, y / PULL_FULL)
      indicator.style.opacity = String(indicatorOpacity)
      indicator.style.transform = reducedMotion
        ? 'none'
        : `translateY(${Math.min(y - PULL_FULL, 0)}px)`
      indicator.setAttribute(
        'aria-hidden',
        !pulling && !pullRefreshingRef.current ? 'true' : 'false'
      )
      indicator
        .querySelector('svg')
        ?.classList.toggle('animate-spin', pullRefreshingRef.current && !reducedMotion)
    }
  }, [])

  const reset = useCallback(
    (animate = true) => {
      if (frameRef.current !== null) cancelAnimationFrame(frameRef.current)
      frameRef.current = null
      mobileOffsetRef.current = 0
      pullOffsetRef.current = 0
      draw(animate)
    },
    [draw]
  )

  const onTouchMove = useCallback(
    (event: React.TouchEvent<HTMLDivElement>) => {
      const start = startRef.current
      if (!start || event.touches.length !== 1) return
      const touch = event.touches[0]
      const deltaX = touch.clientX - start.x
      const deltaY = touch.clientY - start.y
      const canSwitch =
        nodeAfterSwipe(selectedNode, nodes, deltaX < 0 ? -SWIPE_EDGE : SWIPE_EDGE, 0) !== undefined
      const nextX = swipeDragOffset(deltaX, deltaY, canSwitch)
      const nextY = pullDragOffset(deltaX, deltaY, start.canPull)
      if (nextX === mobileOffsetRef.current && nextY === pullOffsetRef.current) return
      const previousX = mobileOffsetRef.current
      const previousY = pullOffsetRef.current
      mobileOffsetRef.current = nextX
      pullOffsetRef.current = nextY
      if (nextY > 0 && labelRef.current) {
        labelRef.current.textContent = shouldRefreshOnPull(deltaX, deltaY, start.canPull)
          ? 'Release to refresh'
          : 'Pull to refresh'
      }
      if (nextX === 0 && nextY === 0 && (previousX !== 0 || previousY !== 0)) {
        reset()
        return
      }
      if (frameRef.current !== null) return
      frameRef.current = requestAnimationFrame(() => {
        frameRef.current = null
        draw(false)
      })
    },
    [draw, nodes, reset, selectedNode]
  )

  const onTouchStart = useCallback(
    (event: React.TouchEvent<HTMLDivElement>) => {
      startRef.current = null
      if (pullRefreshingRef.current) return
      reset(false)
      lastSwipeAtRef.current = 0
      if (event.touches.length !== 1) return
      const target = event.target
      const x = event.touches[0].clientX
      const interactive =
        target instanceof Element &&
        target.closest(
          '[data-node-swipe-ignore], button, a, input, textarea, select, [role="button"]'
        )
      if (x < 24 || x > window.innerWidth - 24 || interactive) return
      const canPull =
        !loading &&
        !refreshing &&
        event.currentTarget.scrollTop <= 0 &&
        (document.scrollingElement?.scrollTop ?? window.scrollY) <= 0
      startRef.current = { x, y: event.touches[0].clientY, canPull }
    },
    [loading, refreshing, reset]
  )

  const shouldSuppressClick = useCallback(() => {
    if (lastSwipeAtRef.current && Date.now() - lastSwipeAtRef.current < GESTURE_CLICK_SUPPRESS_MS) {
      lastSwipeAtRef.current = 0
      return true
    }
    return false
  }, [])

  const onTouchCancel = useCallback(() => {
    startRef.current = null
    if (!pullRefreshingRef.current) reset()
  }, [reset])

  const onTouchEnd = useCallback(
    (event: React.TouchEvent<HTMLDivElement>) => {
      const start = startRef.current
      startRef.current = null
      if (!start || event.changedTouches.length !== 1) {
        if (!pullRefreshingRef.current) reset()
        return
      }
      const touch = event.changedTouches[0]
      const deltaX = touch.clientX - start.x
      const deltaY = touch.clientY - start.y
      if (shouldRefreshOnPull(deltaX, deltaY, start.canPull) && !loading && !refreshing) {
        if (frameRef.current !== null) cancelAnimationFrame(frameRef.current)
        frameRef.current = null
        mobileOffsetRef.current = 0
        pullOffsetRef.current = PULL_HELD_OFFSET
        pullRefreshingRef.current = true
        if (labelRef.current) labelRef.current.textContent = 'Refreshing…'
        draw(true)
        lastSwipeAtRef.current = Date.now()
        void onRefresh().finally(() => {
          pullRefreshingRef.current = false
          reset()
          if (labelRef.current) labelRef.current.textContent = 'Pull to refresh'
        })
        return
      }
      const wasPulling = pullOffsetRef.current > 0
      reset()
      // Decide the navigation outcome first: a clean horizontal swipe should
      // always win over a stray vertical drift, even if the pull visual
      // flickered briefly mid-gesture.
      const next = nodeAfterSwipe(selectedNode, nodes, deltaX, deltaY)
      if (next !== undefined) {
        lastSwipeAtRef.current = Date.now()
        onNodeChange(next)
        return
      }
      // No node change. If the user pulled at all, suppress the click on the
      // card below so a slightly-stale finger-drag doesn't open a session.
      if (wasPulling) {
        lastSwipeAtRef.current = Date.now()
      }
    },
    [draw, loading, nodes, onNodeChange, onRefresh, refreshing, reset, selectedNode]
  )

  return {
    onTouchStart,
    onTouchMove,
    onTouchEnd,
    onTouchCancel,
    noteNonTouchPointer,
    shouldSuppressClick,
    contentRef,
    indicatorRef,
    labelRef,
  }
}
