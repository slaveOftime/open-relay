/**
 * The xterm color palette for light and dark.
 *
 * xterm applies one theme at a time, so `getTerminalTheme()` resolves the
 * scheme exactly like `index.css` does — an explicit `data-theme` on <html>
 * wins over `prefers-color-scheme`. `subscribeTerminalColorScheme()` reports
 * either source changing, so an attached terminal repaints with the rest of
 * the app. Moved out of XTerm.tsx as data so the renderer holds rendering only.
 */

import type { ITheme } from '@xterm/xterm'

export type TerminalColorScheme = 'light' | 'dark'

/** The scheme the app's tokens are rendering under right now. */
export function resolveTerminalColorScheme(): TerminalColorScheme {
  const explicit = document.documentElement.dataset.theme
  if (explicit === 'light' || explicit === 'dark') return explicit
  return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light'
}

/**
 * Calls `onChange` when the resolved scheme changes: the OS preference flips,
 * or `data-theme` is set, changed, or removed. The scheme in force at
 * subscribe time is not reported.
 */
export function subscribeTerminalColorScheme(
  onChange: (scheme: TerminalColorScheme) => void
): () => void {
  let previous = resolveTerminalColorScheme()
  const report = () => {
    const next = resolveTerminalColorScheme()
    if (next === previous) return
    previous = next
    onChange(next)
  }
  const media = window.matchMedia('(prefers-color-scheme: dark)')
  media.addEventListener('change', report)
  const observer = new MutationObserver(report)
  observer.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] })
  return () => {
    media.removeEventListener('change', report)
    observer.disconnect()
  }
}

export function getTerminalTheme(): ITheme {
  const dark = resolveTerminalColorScheme() === 'dark'
  if (dark) {
    return {
      background: '#030712',
      foreground: '#e5e7eb',
      cursor: '#a5b4fc',
      cursorAccent: '#030712',
      selectionBackground: '#4f46e580',
      black: '#111827',
      red: '#f87171',
      green: '#4ade80',
      yellow: '#fbbf24',
      blue: '#60a5fa',
      magenta: '#c084fc',
      cyan: '#22d3ee',
      white: '#f9fafb',
      brightBlack: '#374151',
      brightRed: '#fca5a5',
      brightGreen: '#86efac',
      brightYellow: '#fde68a',
      brightBlue: '#93c5fd',
      brightMagenta: '#d8b4fe',
      brightCyan: '#67e8f9',
      brightWhite: '#ffffff',
    }
  }
  return {
    background: '#f1f5f9',
    foreground: '#0f172a',
    cursor: '#4338ca',
    cursorAccent: '#f1f5f9',
    selectionBackground: '#6366f140',
    black: '#1e293b',
    red: '#dc2626',
    green: '#16a34a',
    yellow: '#d97706',
    blue: '#2563eb',
    magenta: '#9333ea',
    cyan: '#0891b2',
    white: '#334155',
    brightBlack: '#475569',
    brightRed: '#ef4444',
    brightGreen: '#22c55e',
    brightYellow: '#f59e0b',
    brightBlue: '#3b82f6',
    brightMagenta: '#a855f7',
    brightCyan: '#06b6d4',
    brightWhite: '#0f172a',
  }
}
