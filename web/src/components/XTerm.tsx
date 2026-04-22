import { useRef, useEffect, useImperativeHandle, forwardRef } from 'react'
import { Terminal, type ITheme } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { hasTransferredFiles } from './ui/file-transfer'
// import { CanvasAddon } from '@xterm/addon-canvas';
import '@xterm/xterm/css/xterm.css'

function getTerminalTheme(): ITheme {
  const dark = window.matchMedia('(prefers-color-scheme: dark)').matches
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

export interface XTermHandle {
  write(data: string | Uint8Array, callback?: () => void): void
  writeln(data: string): void
  clear(): void
  reset(): void
  resize(cols: number, rows: number): void
  scrollToBottom(): void
  scrollToTop(): void
  scrollLines(amount: number): void
  getSize(): { cols: number; rows: number } | null
  /** Force FitAddon to compute the correct size immediately and return it. */
  fit(): { cols: number; rows: number } | null
}

interface Props {
  autoFit: boolean
  /** Called with raw keyboard data from xterm (use for WebSocket sendInput) */
  onData?: (data: string) => void
  /** Called when clipboard paste targets the terminal. */
  onPaste?: (event: ClipboardEvent) => void
  /** Called when the terminal is resized by FitAddon (cols, rows) */
  onResize?: (cols: number, rows: number) => void
  className?: string
}

const XTerm = forwardRef<XTermHandle, Props>(function XTerm(
  { autoFit, onData, onPaste, onResize, className },
  ref
) {
  const containerRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<Terminal | null>(null)
  const fitRef = useRef<FitAddon | null>(null)
  const onDataRef = useRef(onData)
  const onPasteRef = useRef(onPaste)
  const onResizeRef = useRef(onResize)
  const lastResizeRef = useRef<{ cols: number; rows: number } | null>(null)

  // Keep callbacks up to date without re-running the mount effect
  useEffect(() => {
    onDataRef.current = onData
  }, [onData])
  useEffect(() => {
    onPasteRef.current = onPaste
  }, [onPaste])
  useEffect(() => {
    onResizeRef.current = onResize
  }, [onResize])

  useImperativeHandle(ref, () => ({
    write(data: string | Uint8Array, callback?: () => void) {
      termRef.current?.write(data, callback)
    },
    writeln(data: string) {
      termRef.current?.writeln(data)
    },
    clear() {
      termRef.current?.clear()
    },
    reset() {
      termRef.current?.reset()
    },
    resize(cols: number, rows: number) {
      if (
        !termRef.current ||
        cols <= 0 ||
        rows <= 0 ||
        (cols === termRef.current.cols && rows === termRef.current.rows)
      ) {
        return
      }
      console.debug(`Resizing xterm to ${cols} cols and ${rows} rows`)
      termRef.current.resize(cols, rows)
      lastResizeRef.current = { cols, rows }
    },
    scrollToBottom() {
      termRef.current?.scrollToBottom()
    },
    scrollToTop() {
      termRef.current?.scrollToTop()
    },
    scrollLines(amount: number) {
      termRef.current?.scrollLines(amount)
    },
    getSize() {
      if (!termRef.current) return null
      return { cols: termRef.current.cols, rows: termRef.current.rows }
    },
    fit() {
      if (!termRef.current || !fitRef.current) return null
      try {
        fitRef.current.fit()
      } catch {
        return null
      }
      return { cols: termRef.current.cols, rows: termRef.current.rows }
    },
  }))

  useEffect(() => {
    const term = termRef.current
    if (!term) return

    const interactive = Boolean(onData)
    term.options.disableStdin = !interactive
    term.options.cursorBlink = interactive

    if (!interactive) {
      term.blur()
    }
  }, [onData])

  useEffect(() => {
    if (!containerRef.current) return

    const term = new Terminal({
      theme: getTerminalTheme(),
      fontFamily:
        'ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace',
      fontSize: 13,
      lineHeight: 1,
      cursorBlink: true,
      cursorStyle: 'block',
      customGlyphs: true,
      scrollback: 1000,
      disableStdin: !onDataRef.current,
      macOptionClickForcesSelection: true,
    })

    term.open(containerRef.current)
    termRef.current = term
    lastResizeRef.current = null

    if (autoFit) {
      const fitAddon = new FitAddon()
      term.loadAddon(fitAddon)
      fitRef.current = fitAddon
    }

    const emitResizeIfChanged = () => {
      const next = { cols: term.cols, rows: term.rows }
      const prev = lastResizeRef.current
      if (prev && prev.cols === next.cols && prev.rows === next.rows) return
      lastResizeRef.current = next
      onResizeRef.current?.(next.cols, next.rows)
    }

    // Defer the initial fit so the renderer has completed its first frame
    let initialRaf = requestAnimationFrame(() => {
      initialRaf = 0
      if (!termRef.current) return
      try {
        fitRef.current?.fit()
        emitResizeIfChanged()
      } catch {
        /* ignore if already disposed */
      }
    })

    // Forward keyboard data
    const dataDisposable = term.onData((data) => {
      onDataRef.current?.(data)
    })

    // Resize observer — also deferred so it never races the renderer
    let pendingRaf = 0
    const ro = new ResizeObserver(() => {
      if (pendingRaf) cancelAnimationFrame(pendingRaf)
      pendingRaf = requestAnimationFrame(() => {
        pendingRaf = 0
        if (!termRef.current) return
        try {
          fitRef.current?.fit()
          emitResizeIfChanged()
        } catch {
          /* ignore during unmount */
        }
      })
    })
    ro.observe(containerRef.current)

    const findScrollContainer = (node: HTMLElement | null): HTMLElement | null => {
      let current = node?.parentElement ?? null
      while (current) {
        const style = window.getComputedStyle(current)
        const overflowY = style.overflowY
        const canScroll =
          (overflowY === 'auto' || overflowY === 'scroll') &&
          current.scrollHeight > current.clientHeight
        if (canScroll) {
          return current
        }
        current = current.parentElement
      }

      return document.scrollingElement instanceof HTMLElement
        ? document.scrollingElement
        : document.documentElement
    }

    let keyboardSyncRaf = 0
    let keyboardSyncPasses = 0
    const maxKeyboardSyncPasses = 8
    const syncFocusedTerminalIntoView = (): boolean => {
      const textarea = term.textarea
      if (!textarea || document.activeElement !== textarea) return false

      const viewport = window.visualViewport
      const viewportBottom = viewport
        ? viewport.offsetTop + viewport.height
        : window.innerHeight
      const rect = container.getBoundingClientRect()
      const bottomPadding = 40
      const overlap = rect.bottom + bottomPadding - viewportBottom
      if (overlap <= 0) return false

      const scrollContainer = findScrollContainer(container)
      if (!scrollContainer) return false

      const scrollTop = Math.ceil(overlap)

      if (scrollContainer === document.documentElement || scrollContainer === document.body) {
        window.scrollBy({ top: scrollTop, behavior: 'auto' })
        return true
      }

      scrollContainer.scrollBy({ top: scrollTop, behavior: 'auto' })
      return true
    }

    const scheduleKeyboardSync = () => {
      if (keyboardSyncRaf) cancelAnimationFrame(keyboardSyncRaf)
      keyboardSyncRaf = requestAnimationFrame(() => {
        keyboardSyncRaf = 0
        const didScroll = syncFocusedTerminalIntoView()
        if (didScroll && keyboardSyncPasses < maxKeyboardSyncPasses) {
          keyboardSyncPasses += 1
          scheduleKeyboardSync()
          return
        }
        keyboardSyncPasses = 0
      })
    }

    // iOS PWA: tapping the terminal canvas doesn't reliably trigger the
    // virtual keyboard in standalone mode. Explicitly focus xterm's internal
    // input element on touchend so the keyboard appears.
    const container = containerRef.current
    const handleTouchEnd = () => {
      if (!onDataRef.current) return
      keyboardSyncPasses = 0
      term.focus()
      scheduleKeyboardSync()
    }
    const handleTerminalFocus = () => {
      keyboardSyncPasses = 0
      scheduleKeyboardSync()
    }
    const handlePaste = (event: ClipboardEvent) => {
      const clipboardData = event.clipboardData
      if (!onPasteRef.current || !clipboardData) return

      if (hasTransferredFiles(clipboardData)) {
        event.stopPropagation()
        onPasteRef.current(event)
        return
      }

      if (clipboardData.getData('text/plain')) {
        return
      }

      event.stopPropagation()
      onPasteRef.current(event)
    }
    const viewport = window.visualViewport
    container.addEventListener('touchend', handleTouchEnd, { passive: true })
    term.textarea?.addEventListener('focus', handleTerminalFocus)
    term.textarea?.addEventListener('paste', handlePaste, true)
    viewport?.addEventListener('resize', scheduleKeyboardSync)
    viewport?.addEventListener('scroll', scheduleKeyboardSync)

    return () => {
      // Null refs immediately so any in-flight callbacks become no-ops
      termRef.current = null
      fitRef.current = null
      lastResizeRef.current = null
      // Cancel our own pending RAFs
      if (initialRaf) cancelAnimationFrame(initialRaf)
      if (pendingRaf) cancelAnimationFrame(pendingRaf)
      if (keyboardSyncRaf) cancelAnimationFrame(keyboardSyncRaf)
      dataDisposable.dispose()
      ro.disconnect()
      container.removeEventListener('touchend', handleTouchEnd)
      term.textarea?.removeEventListener('focus', handleTerminalFocus)
      term.textarea?.removeEventListener('paste', handlePaste)
      viewport?.removeEventListener('resize', scheduleKeyboardSync)
      viewport?.removeEventListener('scroll', scheduleKeyboardSync)
      // Defer dispose by TWO frames so xterm's own internally-scheduled
      // RAFs can fully drain before _renderService is torn down.
      requestAnimationFrame(() => requestAnimationFrame(() => term.dispose()))
    }
  }, [autoFit]) // mount only

  // Update terminal theme when OS color scheme changes
  useEffect(() => {
    const mq = window.matchMedia('(prefers-color-scheme: dark)')
    const handler = () => {
      if (termRef.current) {
        termRef.current.options.theme = getTerminalTheme()
      }
    }
    mq.addEventListener('change', handler)
    return () => mq.removeEventListener('change', handler)
  }, [])

  return (
    <div
      ref={containerRef}
      className={className}
      style={{ overflow: 'hidden', touchAction: 'none' }}
    />
  )
})

export default XTerm
