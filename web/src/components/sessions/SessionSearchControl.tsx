/**
 * The sessions page's search control for small screens: a floating trigger at
 * the right edge of the list — the same shape, size and offset as the
 * terminal's scroll handle — so it sits under the thumb and never scrolls away
 * with the list.
 *
 * Collapsed it is that one circular button. A tap grows the search bar out of
 * it to the left (one merged pill, the trigger as its right cap) and focuses
 * the input; a press-and-hold opens the quick menu instead — the filter
 * dropdowns the page passes as `menu`. The two popups are mutually exclusive:
 * the menu opens from a hold, and a tap toggles whichever of them is open.
 */
import { useCallback, useEffect, useRef, useState, type CSSProperties, type ReactNode } from 'react'
import { Cross2Icon, MagnifyingGlassIcon } from '@radix-ui/react-icons'
import { cn } from '@/utils/cn'
import { useLongPress } from '@/hooks/use-long-press'
import { useReducedMotion } from '@/hooks/use-reduced-motion'
import { Input } from '@/components/ui/input'

/** QuickKeysMenu's pop ease: a slight overshoot so the menu feels springy. */
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

  const duration = reducedMotion ? 0 : ENTER_DURATION_MS

  const closeMenu = useCallback(() => {
    setMenuOpen(false)
    setArmed(false)
  }, [])
  const collapseBar = useCallback(() => {
    setExpanded(false)
    setArmed(false)
  }, [])

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

  const hold = useLongPress({
    onPress: handlePress,
    // A hold always takes over with the quick menu, collapsing the bar first.
    onLongPress: () => {
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
  // pointerdowns on the trigger belong to the hold, which toggles the menu
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
    <div className={cn('absolute right-5 bottom-70 z-10 md:hidden', open && 'z-30')}>
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
            onBlur={collapseBar}
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

      {/* The trigger: the bar's right cap, and the hold anchor for the menu. */}
      <button
        ref={iconRef}
        type="button"
        aria-label="Search sessions"
        aria-haspopup="true"
        aria-expanded={menuOpen}
        {...hold}
        // Keyboard activation only. Pointer taps are handled on pointerup by
        // the hold, and the click a held press synthesizes must not fire.
        onClick={(event) => {
          if (event.detail !== 0) return
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
          style={popStyle}
          className={cn(
            'absolute bottom-full right-0 z-40 mb-2 w-60 origin-bottom-right rounded-lg',
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
