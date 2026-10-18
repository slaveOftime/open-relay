/**
 * Where the sessions list was left: the scroll offset of whichever list
 * (mobile cards or desktop table) the user last scrolled.
 *
 * `sessionStorage`, not `localStorage`: a restored offset belongs to this tab's
 * browsing session — leaving for a session detail page and coming back should
 * land where the user was, while a fresh tab still opens at the top of the
 * list. Both lists write the same key because only the visible one can be
 * scrolled; the other is left alone by the restore (it is `display:none`, so
 * assigning to it would clamp to 0).
 *
 * Every read degrades to 0 rather than throwing, because losing a scroll
 * position must not stop the list from rendering.
 */

import { useEffect, useRef, type RefObject } from 'react'

export const SESSIONS_SCROLL_STORAGE_KEY = 'open-relay.webv2.sessions.scroll-top.v1'

export function loadSessionsScrollTop(): number {
  if (typeof window === 'undefined') return 0
  try {
    const raw = window.sessionStorage.getItem(SESSIONS_SCROLL_STORAGE_KEY)
    if (!raw) return 0
    const parsed = Number(raw)
    return Number.isFinite(parsed) && parsed > 0 ? Math.floor(parsed) : 0
  } catch {
    return 0
  }
}

export function saveSessionsScrollTop(top: number): void {
  if (typeof window === 'undefined') return
  try {
    if (!Number.isFinite(top) || top <= 0) {
      window.sessionStorage.removeItem(SESSIONS_SCROLL_STORAGE_KEY)
      return
    }
    window.sessionStorage.setItem(SESSIONS_SCROLL_STORAGE_KEY, String(Math.floor(top)))
  } catch {
    /* ignore */
  }
}

/**
 * Save the list's offset as the user scrolls, and put it back once the list
 * has rows to scroll again.
 *
 * Restoring is one-shot per mount and waits for `hasRows`, because assigning
 * `scrollTop` before the rows exist would just clamp against the skeleton's
 * height. The write is coalesced into one animation frame so a fast scroll
 * does not write storage on every event, and a pending write is flushed on
 * unmount so leaving the page cannot drop the newest position.
 */
export function useSessionsScrollRestoration(
  mobileListRef: RefObject<HTMLDivElement | null>,
  desktopListRef: RefObject<HTMLDivElement | null>,
  hasRows: boolean
): void {
  const restoredRef = useRef(false)

  useEffect(() => {
    const attached = [mobileListRef.current, desktopListRef.current].filter(
      (element): element is HTMLDivElement => element !== null
    )
    let rafId: number | null = null
    let latestTop = 0
    const flush = () => {
      rafId = null
      saveSessionsScrollTop(latestTop)
    }
    const onScroll = (event: Event) => {
      const target = event.target
      if (!(target instanceof HTMLElement)) return
      latestTop = target.scrollTop
      if (rafId === null) rafId = requestAnimationFrame(flush)
    }
    for (const element of attached) element.addEventListener('scroll', onScroll, { passive: true })
    return () => {
      for (const element of attached) element.removeEventListener('scroll', onScroll)
      if (rafId !== null) {
        cancelAnimationFrame(rafId)
        saveSessionsScrollTop(latestTop)
      }
    }
  }, [desktopListRef, mobileListRef])

  useEffect(() => {
    if (restoredRef.current || !hasRows) return
    restoredRef.current = true
    const top = loadSessionsScrollTop()
    if (top <= 0) return
    const rafId = requestAnimationFrame(() => {
      for (const ref of [mobileListRef, desktopListRef]) {
        const element = ref.current
        // `offsetParent` is null for the list `display:none` hides; only the
        // visible list can take the offset (the hidden one would clamp to 0).
        if (element && element.offsetParent !== null) element.scrollTop = top
      }
    })
    return () => cancelAnimationFrame(rafId)
  }, [desktopListRef, hasRows, mobileListRef])
}
