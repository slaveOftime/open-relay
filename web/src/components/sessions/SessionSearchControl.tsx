/**
 * The sessions page's search control for small screens: a floating trigger at
 * the right edge of the list — the same shape, size and offset as the
 * terminal's scroll handle — so it sits under the thumb and never scrolls away
 * with the list.
 *
 * Collapsed it is that one circular button. A tap grows the search bar out of
 * it to the left (one merged pill, the trigger as its right cap) and focuses
 * the input; a double tap opens the quick menu instead — the filter dropdowns
 * the page passes as `menu`, replacing the bar the first tap of the pair just
 * opened. The two popups are mutually exclusive, and a tap toggles whichever
 * of them is up. A drag moves the whole control somewhere better and remembers
 * the spot (`useHoldToDrag` with `armMs: 0`: nothing else owns a drag on this
 * button, so a travelling press is a move — no hold-then-move, which used to
 * fight the hold that opened the menu).
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
  const [expanded, setExpanded] = useState(false)
  const [menuOpen, setMenuOpen] = useState(false)
  const [armed, setArmed] = useState(false)
  const iconRef = useRef<HTMLButtonElement>(null)
  const inputRef = useRef<HTMLInputElement>(null)
  const menuRef = useRef<HTMLDivElement>(null)
  const rootRef = useRef<HTMLDivElement>(null)

  const duration = reducedMotion ? 0 : ENTER_DURATION_MS

  const closeMenu = useCallback(() => {
    setMenuOpen(false)
    setArmed(false)
  }, [])
  const collapseBar = useCallback(() => {
    setExpanded(false)
    setArmed(false)
  }, [])

  // Drag the trigger: the control follows the pointer and the drop is
  // remembered. There is no hold to wait out — the press has no other drag
  // meaning, so any press that travels past the slop is a move, and taking
  // the button closes whichever popup is up.
  const reposition = useHoldToDrag({
    handleRef: iconRef,
    elementRef: rootRef,
    storageKey: POSITION_KEY,
    armMs: 0,
    onDragStart: () => {
      collapseBar()
      closeMenu()
    },
  })

  const open = expanded || menuOpen

  const handlePress = useCallback(() => {
    if (menuOpen) {
      closeMenu()
      return
    }
    if (expanded) {
      collapseBar()
      return
    }
    setExpanded(true)
  }, [closeMenu, collapseBar, expanded, menuOpen])

  const press = useTapGestures({
    onPress: handlePress,
    // The second tap of a pair is the quick menu, replacing the bar the first
    // tap just opened.
    onDoubleTap: () => {
      collapseBar()
      setMenuOpen(true)
    },
  })

  // The menu mounts collapsed and arms on the next frame so its transform
  // transition actually runs (the same pattern QuickKeysMenu uses). The bar
  // animates its width instead and needs no such kick.
  useEffect(() => {
    if (!menuOpen) return
    const raf = requestAnimationFrame(() => setArmed(true))
    return () => cancelAnimationFrame(raf)
  }, [menuOpen])

  // Focus the input once the bar is up: the bar re-opens, so autoFocus (which
  // fires on mount only) cannot do this.
  useEffect(() => {
    if (expanded) inputRef.current?.focus()
  }, [expanded])

  // The menu closes on an outside pointerdown or Escape. Pointerdowns inside a
  // menu select's own dropdown (a Radix popper portal) belong to that select;
  // pointerdowns on the trigger belong to its own tap, which toggles the menu
  // itself.
  useEffect(() => {
    if (!menuOpen) return
    const onPointerDown = (event: PointerEvent) => {
      const { target } = event
      if (!(target instanceof Node)) return
      if (menuRef.current?.contains(target)) return
      if (iconRef.current?.contains(target)) return
      if (target instanceof Element && target.closest('[data-radix-popper-content-wrapper]')) {
        return
      }
      closeMenu()
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') closeMenu()
    }
    document.addEventListener('pointerdown', onPointerDown, true)
    document.addEventListener('keydown', onKeyDown)
    return () => {
      document.removeEventListener('pointerdown', onPointerDown, true)
      document.removeEventListener('keydown', onKeyDown)
    }
  }, [closeMenu, menuOpen])

  const popStyle: CSSProperties = {
    // The translate folds the vertical centering into the inline transform —
    // a Tailwind translate class would be overridden by this. Origin is the
    // right edge, so the menu grows out of the trigger toward the left.
    transform: armed ? 'translateY(-50%) scale(1)' : 'translateY(-50%) scale(0.92)',
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
      className={cn('absolute right-5 bottom-70 z-10 md:hidden', open && 'z-30')}
      style={reposition.style}
    >
      {/* The bar: trigger-sized when closed, grown out of the trigger to the
          left when the search is open. */}
      <div
        className={cn(
          'relative flex h-12 items-center overflow-hidden rounded-full',
          'border border-[hsl(var(--border))] bg-[hsl(var(--muted))] shadow-lg',
          'transition-[width]',
          open ? 'opacity-100' : 'opacity-80'
        )}
        style={{
          width: expanded ? BAR_WIDTH_PX : TRIGGER_SIZE_PX,
          transitionDuration: `${duration}ms`,
        }}
      >
        <div
          className={cn(
            'flex h-full w-56 shrink-0 items-center gap-1 pl-3.5 transition-opacity',
            expanded ? 'opacity-100' : 'invisible opacity-0'
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
              // The tap that just opened this bar moves focus to the trigger
              // as the gesture's default action, which blurs the input right
              // after it was focused. That move is not a dismissal.
              if (event.relatedTarget === iconRef.current) return
              // The bar may already be down: the double tap that opened the
              // menu hid the focused input, and the browser blurs it for that
              // — a frame after the menu's entrance arm. Re-collapsing then
              // would only tear the arm (and the menu's clickability) down.
              if (!expanded) return
              collapseBar()
            }}
            onKeyDown={(event) => {
              if (event.key === 'Escape') collapseBar()
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
              else collapseBar()
            }}
            className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full text-[hsl(var(--muted-foreground))] transition-colors hover:text-[hsl(var(--foreground))]"
          >
            <Cross2Icon className="h-4 w-4" />
          </button>
        </div>
      </div>

      {/* The trigger: the bar's right cap, and the tap/double-tap anchor for
          the menu. */}
      <button
        ref={iconRef}
        type="button"
        aria-label="Search sessions"
        aria-haspopup="true"
        aria-expanded={menuOpen}
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

      {menuOpen && menu && (
        <div
          ref={menuRef}
          data-testid="session-search-menu"
          style={popStyle}
          className={cn(
            // Anchored to the trigger's left, vertically centred on it, with a
            // small gap — it reads as opening out of the trigger toward the
            // list, which is where it points.
            'absolute right-full top-1/2 z-40 mr-2 w-60 origin-right rounded-lg',
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
