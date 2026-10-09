import { useRef, useEffect, useImperativeHandle, forwardRef, useState } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { ChevronsUpDown } from 'lucide-react'
import { hasTransferredFiles } from './ui/file-transfer'
import { cn } from '@/utils/cn'
import QuickKeysMenu from './terminal/QuickKeysMenu'
import QuickKeysDialog from './terminal/QuickKeysDialog'
import { markOwnedTerminalWheel } from './terminal/scroll-wheel'
import { getTerminalTheme } from './terminal/xterm-theme'
import { useTerminalKeyboardSync } from '@/hooks/use-terminal-keyboard-sync'
import {
  TERMINAL_FONT_FAMILY,
  TERMINAL_FONT_SIZE,
  TERMINAL_FONT_VARIANTS,
  TERMINAL_PRELOAD_TEXT,
} from './terminal/xterm-fonts'
import type { QuickKey } from '@/lib/quick-keys'
// import { CanvasAddon } from '@xterm/addon-canvas';
import '@xterm/xterm/css/xterm.css'
import './XTerm.css'

// Joystick-style scroll handle tuning: drag offset beyond the deadzone
// scrolls continuously, with speed proportional to the offset distance.
const SCROLL_DRAG_DEADZONE_PX = 8
const SCROLL_LINES_PER_SECOND_PER_PX = 2

// A press below these limits counts as a tap and toggles the quick-keys ring.
const QUICK_KEYS_TAP_MAX_PX = 12
const QUICK_KEYS_TAP_MAX_MS = 350

function loadEmbeddedTerminalFont(): Promise<void> {
  if (typeof document === 'undefined' || !('fonts' in document)) {
    return Promise.resolve()
  }

  return Promise.all(
    TERMINAL_FONT_VARIANTS.map((font) => document.fonts.load(font, TERMINAL_PRELOAD_TEXT))
  ).then(() => undefined)
}

// Dispatch a synthetic wheel event that behaves like a real wheel over the
// terminal: xterm v6 listens on its scrollable overlay element (and on the
// root element for mouse reporting). Events bubble up, never down, so the
// container itself cannot be used as the target.
function emitTerminalWheel(
  term: Terminal | null,
  deltaY: number,
  deltaMode: number = WheelEvent.DOM_DELTA_LINE
) {
  const element = term?.element
  if (!element) return
  const target =
    element.querySelector('.xterm-scrollable-element') ??
    element.querySelector('.xterm-screen') ??
    element
  const rect = target.getBoundingClientRect()
  const event = new WheelEvent('wheel', {
    deltaY,
    deltaMode,
    clientX: rect.left + rect.width / 2,
    clientY: rect.top + rect.height / 2,
    bubbles: true,
    cancelable: true,
  })
  // Flag it so listeners on the terminal container (e.g. the logs replay
  // scrubber, which runs in the capture phase) ignore our own scrolling.
  markOwnedTerminalWheel(event)
  target.dispatchEvent(event)
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
  /**
   * Called when the user's pinned-to-bottom state changes: live
   * output may only auto-scroll while the user is already at the bottom —
   * scrolling up to read history must never be stolen by new output.
   */
  onScrollBottomChange?: (atBottom: boolean) => void
  className?: string
}

const XTerm = forwardRef<XTermHandle, Props>(function XTerm(
  { autoFit, onData, onPaste, onResize, onScrollBottomChange, className },
  ref
) {
  const containerRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<Terminal | null>(null)
  // Owns the iOS keyboard scroll sync, so the terminal effect below only wires
  // the event handlers that need `term` itself.
  const { restart: restartKeyboardSync } = useTerminalKeyboardSync(termRef, containerRef)
  const fitRef = useRef<FitAddon | null>(null)
  const onDataRef = useRef(onData)
  const onPasteRef = useRef(onPaste)
  const onResizeRef = useRef(onResize)
  const onScrollBottomChangeRef = useRef(onScrollBottomChange)
  const lastResizeRef = useRef<{ cols: number; rows: number } | null>(null)
  const scrollDragRef = useRef<{
    anchorY: number
    currentY: number
    pendingLines: number
    lastTime: number
  } | null>(null)
  const scrollDragRafRef = useRef(0)
  const scrollButtonRef = useRef<HTMLButtonElement>(null)
  const [scrollDragActive, setScrollDragActive] = useState(false)
  const tapRef = useRef<{ x: number; y: number; startedAt: number; moved: boolean } | null>(null)
  const [quickKeysOpen, setQuickKeysOpen] = useState(false)
  const [quickKeysDialogOpen, setQuickKeysDialogOpen] = useState(false)

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
  useEffect(() => {
    onScrollBottomChangeRef.current = onScrollBottomChange
  }, [onScrollBottomChange])

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
      fontFamily: TERMINAL_FONT_FAMILY,
      fontSize: TERMINAL_FONT_SIZE,
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

    const syncTerminalLayout = (refreshRows: boolean) => {
      if (!termRef.current) return

      try {
        fitRef.current?.fit()
        emitResizeIfChanged()

        if (refreshRows && term.rows > 0) {
          term.refresh(0, term.rows - 1)
        }
      } catch {
        /* ignore if already disposed */
      }
    }

    // Defer the initial fit so the renderer has completed its first frame
    let initialRaf = requestAnimationFrame(() => {
      initialRaf = 0
      syncTerminalLayout(false)
    })

    let fontLoadRaf = 0
    // Re-fit after the bundled font loads so terminal metrics stay stable everywhere.
    void loadEmbeddedTerminalFont().then(() => {
      if (!termRef.current) return

      fontLoadRaf = requestAnimationFrame(() => {
        fontLoadRaf = 0
        syncTerminalLayout(true)
      })
    })

    // Forward keyboard data
    // Track whether the user is pinned to the bottom; live writes use this
    // to avoid stealing scroll position/selection.
    const reportBottomState = () => {
      const buffer = term.buffer.active
      onScrollBottomChangeRef.current?.(buffer.viewportY >= buffer.baseY)
    }
    const scrollDisposable = term.onScroll(reportBottomState)
    const writeDisposable = term.onWriteParsed(reportBottomState)

    const dataDisposable = term.onData((data) => {
      onDataRef.current?.(data)
    })

    // Resize observer — also deferred so it never races the renderer
    let pendingRaf = 0
    const ro = new ResizeObserver(() => {
      if (pendingRaf) cancelAnimationFrame(pendingRaf)
      pendingRaf = requestAnimationFrame(() => {
        pendingRaf = 0
        syncTerminalLayout(false)
      })
    })
    ro.observe(containerRef.current)

    // iOS PWA: tapping the terminal canvas doesn't reliably trigger the
    // virtual keyboard in standalone mode. Explicitly focus xterm's internal
    // input element on touchend so the keyboard appears.
    const container = containerRef.current
    const handleTouchEnd = () => {
      if (!onDataRef.current) return
      term.focus()
      restartKeyboardSync()
    }
    const handleTerminalFocus = () => {
      restartKeyboardSync()
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
    container.addEventListener('touchend', handleTouchEnd, { passive: true })
    term.textarea?.addEventListener('focus', handleTerminalFocus)
    term.textarea?.addEventListener('paste', handlePaste, true)

    return () => {
      // Null refs immediately so any in-flight callbacks become no-ops
      termRef.current = null
      fitRef.current = null
      lastResizeRef.current = null
      // Cancel our own pending RAFs
      if (initialRaf) cancelAnimationFrame(initialRaf)
      if (pendingRaf) cancelAnimationFrame(pendingRaf)
      if (fontLoadRaf) cancelAnimationFrame(fontLoadRaf)
      dataDisposable.dispose()
      scrollDisposable.dispose()
      writeDisposable.dispose()
      ro.disconnect()
      container.removeEventListener('touchend', handleTouchEnd)
      term.textarea?.removeEventListener('focus', handleTerminalFocus)
      term.textarea?.removeEventListener('paste', handlePaste)
      // Defer dispose by TWO frames so xterm's own internally-scheduled
      // RAFs can fully drain before _renderService is torn down.
      requestAnimationFrame(() => requestAnimationFrame(() => term.dispose()))
    }
    // `restartKeyboardSync` is a stable callback (refs all the way down), so
    // listing it keeps this effect running once per mount.
  }, [autoFit, restartKeyboardSync])

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

  // Mobile scroll handle: dragging it vertically emits synthetic wheel
  // events on the terminal so scrolling works like a mouse wheel. A native
  // (non-passive) wheel listener forwards touchpad/wheel gestures over the
  // handle to the terminal without scrolling the page.
  useEffect(() => {
    const button = scrollButtonRef.current
    if (!button) return
    const forwardWheel = (event: WheelEvent) => {
      event.preventDefault()
      event.stopPropagation()
      emitTerminalWheel(termRef.current, event.deltaY, event.deltaMode)
    }
    button.addEventListener('wheel', forwardWheel, { passive: false })
    return () => {
      button.removeEventListener('wheel', forwardWheel)
      if (scrollDragRafRef.current) cancelAnimationFrame(scrollDragRafRef.current)
    }
  }, [])

  // Joystick-style loop: while the handle is held, the vertical offset from
  // the press point drives continuous scrolling — offset down scrolls down,
  // offset up scrolls up, speed grows with distance.
  const runScrollDragFrame = (time: number) => {
    const drag = scrollDragRef.current
    if (!drag) {
      scrollDragRafRef.current = 0
      return
    }
    scrollDragRafRef.current = requestAnimationFrame(runScrollDragFrame)
    const dt = Math.min((time - drag.lastTime) / 1000, 0.1)
    drag.lastTime = time
    const offset = drag.currentY - drag.anchorY
    const distance = Math.abs(offset) - SCROLL_DRAG_DEADZONE_PX
    if (distance <= 0) return
    const direction = offset > 0 ? 1 : -1
    drag.pendingLines += direction * distance * SCROLL_LINES_PER_SECOND_PER_PX * dt
    const lines = Math.trunc(drag.pendingLines)
    if (lines === 0) return
    drag.pendingLines -= lines
    emitTerminalWheel(termRef.current, lines)
  }

  const beginScrollDrag = (clientY: number) => {
    scrollDragRef.current = {
      anchorY: clientY,
      currentY: clientY,
      pendingLines: 0,
      lastTime: performance.now(),
    }
    setScrollDragActive(true)
    if (!scrollDragRafRef.current) {
      scrollDragRafRef.current = requestAnimationFrame(runScrollDragFrame)
    }
  }

  /**
   * Park DOM focus on the scroll handle instead of xterm's textarea.
   *
   * Touching the handle must not raise the soft keyboard. xterm keeps its
   * hidden textarea focused, and mobile browsers re-show the IME for that
   * still-focused field on the next tap — even one aimed at our own controls.
   * The handle is a plain button, so focusing it releases the textarea (and the
   * keyboard with it) without any viewport guessing.
   *
   * Returns whether the handle actually took focus. It is `md:hidden`, so on
   * desktop the call is a no-op and callers fall back to the terminal.
   */
  const focusScrollButton = () => {
    const button = scrollButtonRef.current
    if (!button) return false
    button.focus({ preventScroll: true })
    return document.activeElement === button
  }

  const handleScrollDragStart = (event: React.PointerEvent<HTMLButtonElement>) => {
    event.preventDefault()
    event.stopPropagation()
    focusScrollButton()
    event.currentTarget.setPointerCapture(event.pointerId)
    tapRef.current = { x: event.clientX, y: event.clientY, startedAt: Date.now(), moved: false }
    beginScrollDrag(event.clientY)
  }

  const handleScrollDragMove = (event: React.PointerEvent<HTMLButtonElement>) => {
    const tap = tapRef.current
    if (
      tap &&
      !tap.moved &&
      Math.hypot(event.clientX - tap.x, event.clientY - tap.y) > QUICK_KEYS_TAP_MAX_PX
    ) {
      tap.moved = true
    }
    if (!scrollDragRef.current) {
      // Some touchpads cancel the pointer stream when a drag begins, so
      // (re)start the drag on any pressed move over the handle.
      if (!(event.buttons & 1)) return
      beginScrollDrag(event.clientY)
      return
    }
    event.preventDefault()
    scrollDragRef.current.currentY = event.clientY
  }

  const handleScrollDragEnd = (event: React.PointerEvent<HTMLButtonElement>) => {
    const tap = tapRef.current
    tapRef.current = null
    if (tap && !tap.moved && Date.now() - tap.startedAt <= QUICK_KEYS_TAP_MAX_MS) {
      setQuickKeysOpen((open) => !open)
    }
    if (!scrollDragRef.current) return
    scrollDragRef.current = null
    setScrollDragActive(false)
    if (scrollDragRafRef.current) {
      cancelAnimationFrame(scrollDragRafRef.current)
      scrollDragRafRef.current = 0
    }
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId)
    }
  }

  useEffect(() => {
    if (!quickKeysOpen) return
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setQuickKeysOpen(false)
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [quickKeysOpen])

  const sendQuickKey = (key: QuickKey) => {
    const term = termRef.current
    if (!term) return
    // input() routes through term.onData — the same path as typed keys.
    term.input(key.data)
    // Park focus on the handle: the command is already sent, and re-raising
    // the keyboard for every tap is not what the ring is for. The handle is
    // md:hidden, so desktop falls back to focusing the terminal.
    if (!focusScrollButton() && onDataRef.current) term.focus()
  }

  return (
    <div className={`${cn('relative', className)}`}>
      <div
        ref={containerRef}
        className="h-full w-full"
        style={{ overflow: 'hidden', touchAction: 'none' }}
      />
      {quickKeysOpen ? (
        <div
          aria-hidden
          className="fixed inset-0 z-20"
          onClick={(event) => {
            // Dismiss layer: only close the ring, never reach page handlers.
            event.stopPropagation()
            setQuickKeysOpen(false)
          }}
        />
      ) : null}
      <div
        data-terminal-scroll-controls
        className={cn('absolute right-5 bottom-70 z-10 md:hidden', quickKeysOpen && 'z-30')}
      >
        <button
          ref={scrollButtonRef}
          type="button"
          aria-label="Scroll terminal"
          aria-expanded={quickKeysOpen}
          onPointerDown={handleScrollDragStart}
          onPointerMove={handleScrollDragMove}
          onPointerUp={handleScrollDragEnd}
          onPointerCancel={handleScrollDragEnd}
          onClick={(event) => event.stopPropagation()}
          onContextMenu={(event) => event.preventDefault()}
          className={cn(
            'relative flex h-12 w-12 touch-none select-none items-center justify-center rounded-full border border-[hsl(var(--border))] bg-[hsl(var(--muted))] text-[hsl(var(--muted-foreground))] shadow-lg transition-opacity md:hidden',
            scrollDragActive ? 'opacity-90' : 'opacity-80'
          )}
        >
          <ChevronsUpDown className="h-4 w-4" />
        </button>
        <QuickKeysMenu
          open={quickKeysOpen}
          onSend={sendQuickKey}
          onClose={() => setQuickKeysOpen(false)}
          onCustomize={() => setQuickKeysDialogOpen(true)}
        />
      </div>
      <QuickKeysDialog
        open={quickKeysDialogOpen}
        onOpenChange={(open) => {
          setQuickKeysDialogOpen(open)
          if (open) return
          // Radix restores focus to whatever opened the dialog on close; the
          // scroll handle is the only sensible target and keeps the keyboard
          // down. Defer a frame so this wins over that restore.
          requestAnimationFrame(() => focusScrollButton())
        }}
      />
    </div>
  )
})

export default XTerm
