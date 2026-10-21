/**
 * The live height of the soft keyboard, for pages that own the whole viewport.
 *
 * `window.innerHeight` keeps the keyboard's height on Android Chrome (and
 * WebKit), so a layout rooted in `100dvh` stays full-screen and paints its
 * bottom rows underneath the IME. Subtracting this inset from the page shell
 * gives the terminal the visible height: FitAddon drops rows, ResizeObserver
 * reports the new size, and the bottom row — where the cursor sits while you
 * type — is restored above the keyboard.
 *
 * The reading comes from `visualViewport`, the only cross-browser report of
 * the area the keyboard has not covered. It is a numeric snapshot so
 * `useSyncExternalStore` can compare readings by value and re-render only when
 * the measured overlap actually changes.
 */

import { useSyncExternalStore } from 'react'

import { softKeyboardInset } from '@/utils/soft-keyboard'

function readInset(): number {
  if (typeof window === 'undefined' || typeof document === 'undefined') return 0

  const layoutHeight = document.documentElement.clientHeight || window.innerHeight
  return softKeyboardInset(layoutHeight, window.visualViewport)
}

function subscribe(onChange: () => void): () => void {
  if (typeof window === 'undefined') return () => {}

  const viewport = window.visualViewport
  window.addEventListener('resize', onChange)
  window.addEventListener('orientationchange', onChange)
  viewport?.addEventListener('resize', onChange)
  // `offsetTop` moves when the browser pans a focused field into view without
  // changing its height; the inset depends on that offset too.
  viewport?.addEventListener('scroll', onChange)

  return () => {
    window.removeEventListener('resize', onChange)
    window.removeEventListener('orientationchange', onChange)
    viewport?.removeEventListener('resize', onChange)
    viewport?.removeEventListener('scroll', onChange)
  }
}

export function useSoftKeyboardInset(): number {
  return useSyncExternalStore(subscribe, readInset, () => 0)
}
