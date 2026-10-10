/**
 * The sessions page's search control for small screens: a floating trigger at
 * the bottom-right of the list — the same shape and position as the terminal's
 * scroll handle, so it sits under the thumb and never scrolls away with the
 * list.
 *
 * A tap expands a search bar above the trigger and focuses the input; a
 * press-and-hold opens the quick menu instead — the filter dropdowns the page
 * passes as `menu`. Both popups anchor above the trigger, and the press itself
 * decides which one appears: a hold always takes the menu, a tap toggles
 * whichever is open.
 */
import { useCallback, useEffect, useRef, useState, type CSSProperties, type ReactNode } from 'react'
import { Cross2Icon, MagnifyingGlassIcon } from '@radix-ui/react-icons'
import { cn } from '@/utils/cn'
import { useLongPress } from '@/hooks/use-long-press'
import { useReducedMotion } from '@/hooks/use-reduced-motion'
import { Input } from '@/components/ui/input'

/** QuickKeysMenu's pop ease: a slight overshoot so the popups feel springy. */
const SPRING_EASE = 'cubic-bezier(0.34, 1.56, 0.64, 1)'
const ENTER_DURATION_MS = 200

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
  // Popups mount collapsed and arm on the next frame so the transform
  // transition actually runs (same pattern as QuickKeysMenu).
  const [armed, setArmed] = useState(false)
  const iconRef = useRef<HTMLButtonElement>(null)
  const inputRef = useRef<HTMLInputElement>(null)
  const menuRef = useRef<HTMLDivElement>(null)

  // Closing disarms immediately (not in an effect — a state set in an effect's
  // closed branch cascades); opening arms on the next frame.
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
    // A hold always takes over with the quick menu, closing the search bar.
    onLongPress: () => {
      setExpanded(false)
      setMenuOpen(true)
    },
  })

  useEffect(() => {
    if (!open) return
    const raf = requestAnimationFrame(() => setArmed(true))
    return () => cancelAnimationFrame(raf)
  }, [open])

  // Focus the input once the bar is up: it re-opens, so autoFocus (which fires
  // on mount only) cannot do this.
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

  const duration = reducedMotion ? 0 : ENTER_DURATION_MS
  const popStyle: CSSProperties = {
    transform: armed ? 'scale(1)' : 'scale(0.92)',
    opacity: armed ? 1 : 0,
    transitionDuration: `${duration}ms`,
    transitionTimingFunction: SPRING_EASE,
  }

  return (
    <div className={cn('absolute right-5 bottom-20 z-10 md:hidden', open && 'z-30')}>
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
          'relative flex h-12 w-12 touch-none select-none items-center justify-center',
          'rounded-full border border-[hsl(var(--border))] bg-[hsl(var(--muted))]',
          'text-[hsl(var(--muted-foreground))] shadow-lg transition-opacity',
          open ? 'opacity-100' : 'opacity-80',
          active && 'text-[hsl(var(--primary))]'
        )}
      >
        <MagnifyingGlassIcon className="h-4 w-4" />
        {active && (
          <span className="absolute top-1.5 right-1.5 h-1.5 w-1.5 rounded-full bg-[hsl(var(--primary))]" />
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

      {expanded && (
        <div
          className={cn(
            'absolute bottom-full right-0 z-40 mb-2 flex w-60 items-center gap-1 rounded-full',
            'border border-[hsl(var(--border))] bg-[hsl(var(--muted))] p-1 pl-3 shadow-lg',
            'transition-opacity',
            armed ? 'opacity-100' : 'opacity-0'
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
            className="h-8 min-w-0 flex-1 border-0 bg-transparent px-0 text-sm shadow-none focus-visible:ring-0"
            placeholder="Search cmd: cwd: title: tag:"
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
            <Cross2Icon className="h-3.5 w-3.5" />
          </button>
        </div>
      )}
    </div>
  )
}
