/**
 * iOS soft-keyboard scroll sync.
 *
 * When the virtual keyboard opens it covers the bottom of the viewport, and a
 * terminal that is partially underneath it needs to be scrolled into view.
 * `window.visualViewport` is the only reliable signal for how much of the page
 * the keyboard is currently covering, so the hook watches it and scrolls the
 * nearest scrollable container by the overlap.
 *
 * One scroll can change the layout enough to move the terminal again, so the
 * sync re-schedules itself up to `MAX_PASSES` times; the cap exists because a
 * scroll that keeps producing overlap would otherwise loop forever.
 */

import { useCallback, useEffect, useRef } from 'react'

import type { Terminal } from '@xterm/xterm'

import { findScrollContainer } from '@/utils/scroll-container'

const MAX_PASSES = 8
const BOTTOM_PADDING = 40

export interface TerminalKeyboardSync {
  /** Run one sync pass on the next frame, without resetting the pass budget. */
  schedule: () => void
  /** Restart the pass budget and schedule — the touchend / focus entry points. */
  restart: () => void
  /** Drop any frame already scheduled. */
  cancel: () => void
}

export function useTerminalKeyboardSync(
  termRef: React.RefObject<Terminal | null>,
  containerRef: React.RefObject<HTMLElement | null>
): TerminalKeyboardSync {
  const rafRef = useRef(0)

  const syncFocusedTerminalIntoView = useCallback((): boolean => {
    const term = termRef.current
    const container = containerRef.current
    if (!term || !container) return false

    const textarea = term.textarea
    if (!textarea || document.activeElement !== textarea) return false

    const viewport = window.visualViewport
    const viewportBottom = viewport ? viewport.offsetTop + viewport.height : window.innerHeight
    const rect = container.getBoundingClientRect()
    const overlap = rect.bottom + BOTTOM_PADDING - viewportBottom
    if (overlap <= 0) return false

    const scrollContainer = findScrollContainer(container)
    if (!scrollContainer) return false

    const scrollTop = Math.ceil(overlap)
    // When the nearest scrollable element is the document itself, scrolling it
    // goes through `window`, not `scrollBy` on the element.
    if (scrollContainer === document.documentElement || scrollContainer === document.body) {
      window.scrollBy({ top: scrollTop, behavior: 'auto' })
      return true
    }

    scrollContainer.scrollBy({ top: scrollTop, behavior: 'auto' })
    return true
  }, [containerRef, termRef])

  const schedule = useCallback(() => {
    // The pass budget is a local depth rather than a ref: nothing reads it
    // outside this loop, and taking it as a parameter avoids a useCallback that
    // has to reference itself.
    const run = (depth: number) => {
      if (rafRef.current) cancelAnimationFrame(rafRef.current)
      rafRef.current = requestAnimationFrame(() => {
        rafRef.current = 0
        const didScroll = syncFocusedTerminalIntoView()
        if (didScroll && depth < MAX_PASSES) {
          run(depth + 1)
          return
        }
      })
    }
    run(0)
  }, [syncFocusedTerminalIntoView])

  // A fresh gesture starts its pass budget over; with the budget carried as a
  // local depth, that is just a fresh schedule().
  const restart = useCallback(() => {
    schedule()
  }, [schedule])

  const cancel = useCallback(() => {
    if (rafRef.current) cancelAnimationFrame(rafRef.current)
    rafRef.current = 0
  }, [])

  useEffect(() => {
    const viewport = window.visualViewport
    if (!viewport) return
    // `schedule`, not `restart`: a viewport change continues the current
    // gesture's pass budget rather than restarting it.
    viewport.addEventListener('resize', schedule)
    viewport.addEventListener('scroll', schedule)
    return () => {
      viewport.removeEventListener('resize', schedule)
      viewport.removeEventListener('scroll', schedule)
      cancel()
    }
  }, [cancel, schedule])

  return { schedule, restart, cancel }
}
