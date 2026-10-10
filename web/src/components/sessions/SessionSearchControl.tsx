/**
 * The sessions header's search control for small screens.
 *
 * Collapsed it is one search icon. A tap expands a search bar over the header
 * row and focuses the input; a press-and-hold opens the quick menu instead —
 * the filter dropdowns the page passes as `menu` — so changing group, status
 * or sort is one long press away rather than behind a toggle button.
 *
 * The bar and the menu are mutually exclusive by construction: the menu opens
 * from a hold on the icon, and once expanded the icon is covered so no hold
 * can start. Opening one still closes the other.
 */
import { useCallback, useEffect, useRef, useState, type CSSProperties, type ReactNode } from 'react'
import { Cross2Icon, MagnifyingGlassIcon } from '@radix-ui/react-icons'
import { cn } from '@/utils/cn'
import { useLongPress } from '@/hooks/use-long-press'
import { useReducedMotion } from '@/hooks/use-reduced-motion'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'

/** QuickKeysMenu's pop ease: a slight overshoot so the menu feels springy. */
const SPRING_EASE = 'cubic-bezier(0.34, 1.56, 0.64, 1)'
const ENTER_DURATION_MS = 200

interface Props {
  search: string
  onSearchChange: (value: string) => void
  /** Whether any filter is active; draws the dot on the collapsed icon. */
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
  // closed branch cascades); opening arms on the next frame so the transform
  // transition actually runs (same pattern as QuickKeysMenu).
  const closeMenu = useCallback(() => {
    setMenuOpen(false)
    setArmed(false)
  }, [])
  const collapseBar = useCallback(() => {
    setExpanded(false)
    setArmed(false)
  }, [])

  const hold = useLongPress({
    // A tap expands the bar — but only when the menu is closed: a tap on the
    // icon while the menu is open dismisses the menu instead. Opening one
    // closes the other so the two popups can never stack.
    onPress: () => {
      if (menuOpen) {
        closeMenu()
        return
      }
      setExpanded(true)
    },
    onLongPress: () => {
      setExpanded(false)
      setMenuOpen(true)
    },
  })

  useEffect(() => {
    if (!expanded && !menuOpen) return
    const raf = requestAnimationFrame(() => setArmed(true))
    return () => cancelAnimationFrame(raf)
  }, [expanded, menuOpen])

  // Focus the input once the bar is up. The bar re-opens, so autoFocus (which
  // fires on mount only) cannot do this.
  useEffect(() => {
    if (expanded) inputRef.current?.focus()
  }, [expanded])

  // The menu closes on an outside pointerdown or Escape. Pointerdowns inside a
  // menu select's own dropdown (a Radix popper portal) belong to that select;
  // pointerdowns on the icon belong to the hold, which dismisses the menu on
  // tap itself.
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
    <>
      <Button
        ref={iconRef}
        type="button"
        variant="ghost"
        size="icon"
        aria-label="Search sessions"
        aria-haspopup="true"
        aria-expanded={menuOpen}
        {...hold}
        // Keyboard activation only. Pointer taps are handled on pointerup by
        // the hold, and the click a held press synthesizes must not fire.
        onClick={(event) => {
          if (event.detail !== 0) return
          setExpanded(true)
        }}
        className={cn(
          'relative touch-none select-none',
          active && 'text-[hsl(var(--primary))] bg-[hsl(var(--primary))]/10'
        )}
      >
        <MagnifyingGlassIcon className="h-4 w-4" />
        {active && (
          <span className="absolute top-1 right-1 h-1.5 w-1.5 rounded-full bg-[hsl(var(--primary))]" />
        )}
      </Button>

      {menuOpen && menu && (
        <div
          ref={menuRef}
          style={popStyle}
          className={cn(
            'absolute right-0 top-full z-40 mt-2 w-60 origin-top-right rounded-lg',
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
            'absolute inset-0 z-40 flex items-center gap-2 bg-[hsl(var(--background))] px-3',
            'transition-opacity',
            armed ? 'opacity-100' : 'opacity-0'
          )}
          style={{ transitionDuration: `${duration}ms` }}
          // Tapping the bar's empty area is not a tap elsewhere: keep focus in
          // the input, so the blur-close cannot fire and a hidden header
          // button underneath cannot take the press.
          onPointerDown={(event) => {
            if (event.target === event.currentTarget) event.preventDefault()
          }}
        >
          <Input
            ref={inputRef}
            className="h-8 min-w-0 flex-1 text-sm"
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
            className="flex h-8 w-8 shrink-0 items-center justify-center rounded-md text-[hsl(var(--muted-foreground))] transition-colors hover:text-[hsl(var(--foreground))]"
          >
            <Cross2Icon className="h-3.5 w-3.5" />
          </button>
        </div>
      )}
    </>
  )
}
