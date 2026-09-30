import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  useSyncExternalStore,
  type CSSProperties,
} from 'react'
import { SlidersHorizontal } from 'lucide-react'
import { cn } from '@/lib/utils'
import { getQuickKeys, subscribeQuickKeys } from '@/lib/quickKeysStorage'
import { useReducedMotion } from './use-reduced-motion'
import type { QuickKey } from './quick-keys'

const BASE_RADIUS_PX = 92
const MIN_RADIUS_PX = 64
/** Minimum center-to-center distance so adjacent buttons stay 44px targets. */
const MIN_SPACING_PX = 52
/** Keep this much clearance between popped keys and the screen edges. */
const EDGE_MARGIN_PX = 10
const ITEM_HALF_PX = 22
const ENTER_DURATION_MS = 260
const STAGGER_MS = 24
/** Slight overshoot so keys feel like they "pop" out of the pad. */
const SPRING_EASE = 'cubic-bezier(0.34, 1.56, 0.64, 1)'

interface Props {
  /** Whether the radial is expanded. The menu mounts only while open. */
  open: boolean
  onSend: (key: QuickKey) => void
  onClose: () => void
  onCustomize: () => void
}

/**
 * Fan the ring over a half circle on the free side of the pad: straight up,
 * around the left, to straight down. The pad hugs the right screen edge, so
 * the right quarter is deliberately left empty. The last (bottom) slot is
 * reserved for the customize button.
 */
function arcPosition(index: number, count: number, radius: number) {
  const angle = count > 1 ? -Math.PI / 2 - (index / (count - 1)) * Math.PI : Math.PI / 2
  return { dx: Math.cos(angle) * radius, dy: Math.sin(angle) * radius }
}

export default function QuickKeysMenu({ open, onSend, onClose, onCustomize }: Props) {
  const keys = useSyncExternalStore(subscribeQuickKeys, getQuickKeys)
  const reducedMotion = useReducedMotion()
  // Items mount collapsed at the pad center and slide out on the next
  // frame so the CSS transform transition actually runs.
  const [armed, setArmed] = useState(false)
  const anchorRef = useRef<HTMLDivElement>(null)
  const [radius, setRadius] = useState(BASE_RADIUS_PX)
  useEffect(() => {
    if (!open) return
    const raf = requestAnimationFrame(() => setArmed(true))
    return () => cancelAnimationFrame(raf)
  }, [open])

  // Grow the arc so keys keep a tappable spacing, then shrink it back down
  // to whatever fits between the pad center and the screen edges.
  useLayoutEffect(() => {
    const el = anchorRef.current
    if (!el) return
    const { left, top } = el.getBoundingClientRect()
    const freeSpace = Math.min(
      top - EDGE_MARGIN_PX,
      left - EDGE_MARGIN_PX,
      window.innerHeight - top - EDGE_MARGIN_PX
    )
    const wanted = Math.max(BASE_RADIUS_PX, (Math.max(keys.length, 1) * MIN_SPACING_PX) / Math.PI)
    setRadius(Math.max(MIN_RADIUS_PX, Math.min(wanted, freeSpace - ITEM_HALF_PX)))
  }, [keys.length, open])

  if (!open) return null

  const stagger = reducedMotion ? 0 : STAGGER_MS
  const duration = reducedMotion ? 0 : ENTER_DURATION_MS

  const popStyle = (dx: number, dy: number, index: number): CSSProperties => ({
    transform: armed
      ? `translate(calc(-50% + ${dx}px), calc(-50% + ${dy}px)) scale(1)`
      : 'translate(-50%, -50%) scale(0.25)',
    opacity: armed ? 1 : 0,
    transitionDuration: `${duration}ms`,
    transitionTimingFunction: SPRING_EASE,
    transitionDelay: `${index * stagger}ms`,
  })

  // The customize button always occupies the reserved bottom slot, so the
  // arc spreads keys over slots 0..n-1 of n+1 total.
  const slotCount = keys.length + 1
  const customizePos = arcPosition(keys.length, slotCount, radius)

  return (
    // The arc is centered on the pad button (wrapper center); the root
    // itself ignores pointers so the pad underneath stays draggable.
    <div ref={anchorRef} className="pointer-events-none absolute left-1/2 top-1/2">
      {keys.map((key, index) => {
        const { dx, dy } = arcPosition(index, slotCount, radius)
        return (
          <button
            key={key.id}
            type="button"
            aria-label={`Send ${key.label}`}
            onClick={(event) => {
              event.stopPropagation()
              onSend(key)
              onClose()
            }}
            style={popStyle(dx, dy, index)}
            className={cn(
              'pointer-events-auto absolute left-1/2 top-1/2 flex h-11 w-11 touch-none select-none',
              'items-center justify-center rounded-full border border-[hsl(var(--border))]',
              'bg-[hsl(var(--card))]/95 font-mono text-[10px] font-semibold leading-none',
              'text-[hsl(var(--foreground))] shadow-lg backdrop-blur-sm',
              'transition-[transform,opacity] active:opacity-70'
            )}
          >
            {key.label}
          </button>
        )
      })}
      <button
        type="button"
        aria-label="Customize quick keys"
        onClick={(event) => {
          event.stopPropagation()
          onCustomize()
        }}
        style={popStyle(customizePos.dx, customizePos.dy, keys.length)}
        className={cn(
          'pointer-events-auto absolute left-1/2 top-1/2 flex h-11 w-11 touch-none select-none',
          'items-center justify-center rounded-full border border-dashed border-[hsl(var(--border))]',
          'bg-[hsl(var(--muted))]/95 text-[hsl(var(--muted-foreground))] shadow-lg backdrop-blur-sm',
          'transition-[transform,opacity] active:opacity-70'
        )}
      >
        <SlidersHorizontal className="h-4 w-4" />
      </button>
    </div>
  )
}
