/**
 * The sessions page's search control for small screens: a floating trigger at
 * the right edge of the list — the same shape, size and offset as the
 * terminal's scroll handle — so it sits under the thumb and never scrolls away
 * with the list.
 *
 * Collapsed it is that one circular button. A tap opens the whole control: the
 * search bar grows out of the trigger's right edge toward the left (one merged
 * pill, the trigger as its right cap, the icon itself never moving) and takes
 * focus, and the quick menu — the filter dropdowns the page passes as `menu` —
 * pops up under the trigger, right-aligned with it. A second tap closes both,
 * as does a tap outside or Escape. There is no second gesture to wait for: a
 * tap acts on its release. The X in the bar is the search's own control — it
 * clears the text, or closes the control when there is nothing to clear. A drag
 * moves the whole control somewhere better and remembers the spot
 * (`useHoldToDrag` with `armMs: 0`: nothing else owns a drag on this button, so
 * a travelling press is a move — no hold-then-move, which used to fight the
 * hold that opened the menu).
 */
import { useCallback, useEffect, useRef, useState, type CSSProperties, type ReactNode } from 'react'
import { Cross2Icon, MagnifyingGlassIcon } from '@radix-ui/react-icons'
import { cn } from '@/utils/cn'
import { useHoldToDrag } from '@/hooks/use-hold-to-drag'
import { useTapGestures } from '@/hooks/use-tap-gestures'
import { useReducedMotion } from '@/hooks/use-reduced-motion'
import { Input } from '@/components/ui/input'

/** Where a moved trigger is remembered; the default spot needs no entry. */
const POSITION_KEY = 'oly.sessions.searchTrigger.v1'

/** QuickKeysMenu's pop ease: a slight overshoot so the popups feel springy. */
const SPRING_EASE = 'cubic-bezier(0.34, 1.56, 0.64, 1)'
const ENTER_DURATION_MS = 200
/** h-12, the scroll handle's size. The bar is the trigger plus its input zone. */
const TRIGGER_SIZE_PX = 48
const BAR_WIDTH_PX = 274

interface Props {
  search: string
  onSearchChange: (value: string) => void
  /** Whether any filter is active; tints the trigger and draws its dot. */
  active: boolean
  /** Quick menu content — the page's filter dropdowns. */
  menu?: ReactNode
}

export default function SessionSearchControl({ search, onSearchChange, active, menu }: Props) {
  const reducedMotion = useReducedMotion()
  // One state, because the bar and the menu are one popup now: they come up on
  // the same tap and every dismissal takes both. A half-open control — a filter
  // panel under a collapsed bar, or the reverse — would have no tap that could
  // reach it, since the trigger only toggles the pair.
  const [open, setOpen] = useState(false)
  const [armed, setArmed] = useState(false)
  const iconRef = useRef<HTMLButtonElement>(null)
  const inputRef = useRef<HTMLInputElement>(null)
  const rootRef = useRef<HTMLDivElement>(null)

  const duration = reducedMotion ? 0 : ENTER_DURATION_MS

  const close = useCallback(() => {
    setOpen(false)
    setArmed(false)
  }, [])

  // Drag the trigger: the control follows the pointer and the drop is
  // remembered. There is no hold to wait out — the press has no other drag
  // meaning, so any press that travels past the slop is a move, and taking
  // the button closes the control.
  const reposition = useHoldToDrag({
    handleRef: iconRef,
    elementRef: rootRef,
    storageKey: POSITION_KEY,
    armMs: 0,
    onDragStart: close,
  })

  const handlePress = useCallback(() => {
    if (open) {
      close()
      return
    }
    setOpen(true)
  }, [close, open])

  // The tap has no double: the control has no second action to offer a pair,
  // and a tap that waited for one would only feel late.
  const press = useTapGestures({ onPress: handlePress })

  // The menu mounts collapsed and arms on the next frame so its transform
  // transition actually runs (the same pattern QuickKeysMenu uses). The bar
  // animates its width instead and needs no such kick.
  useEffect(() => {
    if (!open) return
    const raf = requestAnimationFrame(() => setArmed(true))
    return () => cancelAnimationFrame(raf)
  }, [open])

  // Focus the input once the bar is up: the bar re-opens, so autoFocus (which
  // fires on mount only) cannot do this.
  useEffect(() => {
    if (open) inputRef.current?.focus()
  }, [open])

  // The control closes on an outside pointerdown or Escape. A pointerdown
  // inside it belongs to it: the bar, the trigger (whose own tap toggles the
  // control), and the menu. A select opens its dropdown in a portal, which
  // belongs to the select.
  useEffect(() => {
    if (!open) return
    const onPointerDown = (event: PointerEvent) => {
      const { target } = event
      if (!(target instanceof Node)) return
      if (rootRef.current?.contains(target)) return
      if (target instanceof Element && target.closest('[data-radix-popper-content-wrapper]')) {
        return
      }
      close()
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') close()
    }
    document.addEventListener('pointerdown', onPointerDown, true)
    document.addEventListener('keydown', onKeyDown)
    return () => {
      document.removeEventListener('pointerdown', onPointerDown, true)
      document.removeEventListener('keydown', onKeyDown)
    }
  }, [close, open])

  const popStyle: CSSProperties = {
    // Only the pop: the placement needs no translate, since the menu hangs from
    // the trigger's bottom-right corner. Origin is that corner, so it grows
    // down and to the left, out of the button it belongs to.
    transform: armed ? 'scale(1)' : 'scale(0.92)',
    opacity: armed ? 1 : 0,
    transitionDuration: `${duration}ms`,
    transitionTimingFunction: SPRING_EASE,
  }

  // The Input's own chrome (border, fill, ring) is stripped: the bar is the
  // control's visible edge, and a ring inside it reads as a box within a box.
  const fieldClass =
    'h-8 min-w-0 flex-1 border-0 bg-transparent px-0 text-sm ' +
    'shadow-none outline-none ring-0 focus-visible:outline-none focus-visible:ring-0 focus-visible:ring-offset-0'

  return (
    <div
      ref={rootRef}
      className={cn('absolute right-5 bottom-70 z-10 h-12 w-12 md:hidden', open && 'z-30')}
      style={reposition.style}
    >
      {/* The bar: trigger-sized when closed, grown out of the trigger's right
          edge toward the left when the search is open. It anchors right so the
          icon never moves — a dragged control is left-anchored, and a bar that
          grew inside the flow would push the icon right, off the screen. */}
      <div
        className={cn(
          'absolute right-0 top-0 flex h-12 items-center overflow-hidden rounded-full',
          'border border-[hsl(var(--border))] bg-[hsl(var(--muted))] shadow-lg',
          'transition-[width]',
          open ? 'opacity-100' : 'opacity-80'
        )}
        style={{
          width: open ? BAR_WIDTH_PX : TRIGGER_SIZE_PX,
          transitionDuration: `${duration}ms`,
        }}
      >
        <div
          className={cn(
            'flex h-full w-56 shrink-0 items-center gap-1 pl-3.5 transition-opacity',
            open ? 'opacity-100' : 'invisible opacity-0'
          )}
          style={{ transitionDuration: `${duration}ms` }}
          // Tapping the bar's padding is not a tap elsewhere: keep focus in the
          // input, so the blur-close cannot dismiss the bar mid-interaction.
          onPointerDown={(event) => {
            if (event.target === event.currentTarget) event.preventDefault()
          }}
        >
          <Input
            ref={inputRef}
            className={fieldClass}
            placeholder="cmd: cwd: title: tag:"
            aria-label="Search sessions by id, title, command, or working directory"
            value={search}
            onChange={(event) => onSearchChange(event.target.value)}
            onBlur={(event) => {
              // The control may already be down: closing it only hides the
              // focused input, and the browser blurs it a frame later.
              if (!open) return
              // Focus staying inside the control is not a dismissal. That
              // covers the trigger — the tap that opened the bar moves focus to
              // it as the gesture's default action — the X button, the menu's
              // own controls, and the dropdown a menu select opens in its
              // portal. The search box staying up while a filter is picked is
              // the point of opening them together.
              const next = event.relatedTarget
              if (next instanceof Node && rootRef.current?.contains(next)) return
              if (next instanceof Element && next.closest('[data-radix-popper-content-wrapper]')) {
                return
              }
              close()
            }}
          />
          <button
            type="button"
            aria-label={search ? 'Clear search' : 'Close search'}
            // preventDefault keeps focus in the input, so the blur-close above
            // cannot unmount this button before its click lands.
            onPointerDown={(event) => event.preventDefault()}
            onClick={() => {
              if (search) onSearchChange('')
              else close()
            }}
            className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full text-[hsl(var(--muted-foreground))] transition-colors hover:text-[hsl(var(--foreground))]"
          >
            <Cross2Icon className="h-4 w-4" />
          </button>
        </div>
      </div>

      {/* The trigger: the bar's right cap, and the control's tap anchor. */}
      <button
        ref={iconRef}
        type="button"
        aria-label="Search sessions"
        aria-haspopup="true"
        aria-expanded={open}
        {...press.handlers}
        // Only a click with no gesture behind it: the keyboard, assistive
        // tech, a programmatic click. The browser's follow-up click for a tap
        // is swallowed by the window check — a tap activates twice without it.
        onClick={(event) => {
          if (press.isFollowUpClick(event.timeStamp)) return
          handlePress()
        }}
        className={cn(
          'absolute right-0 top-0 z-10 flex h-12 w-12 touch-none select-none',
          'items-center justify-center rounded-full',
          'text-[hsl(var(--muted-foreground))] transition-colors',
          active && 'text-[hsl(var(--primary))]'
        )}
      >
        <MagnifyingGlassIcon className="h-5 w-5" />
        {active && (
          <span className="absolute top-1.5 right-1.5 h-2 w-2 rounded-full bg-[hsl(var(--primary))]" />
        )}
      </button>

      {open && menu && (
        <div
          data-testid="session-search-menu"
          style={popStyle}
          className={cn(
            // Hanging under the trigger's bottom-right corner, right-aligned
            // with it and a small gap apart — it reads as opening out of the
            // button, over the list it filters.
            'absolute right-0 top-full z-40 mt-2 w-60 origin-top-right rounded-lg',
            'border border-[hsl(var(--border))] bg-[hsl(var(--card))]/95 p-3',
            'shadow-xl backdrop-blur-sm transition-[transform,opacity]',
            armed ? 'pointer-events-auto' : 'pointer-events-none'
          )}
        >
          {menu}
        </div>
      )}
    </div>
  )
}
