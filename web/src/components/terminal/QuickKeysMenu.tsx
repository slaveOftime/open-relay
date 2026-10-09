import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  useSyncExternalStore,
  type CSSProperties,
} from 'react'
import { SlidersHorizontal } from 'lucide-react'
import { cn } from '@/utils/cn'
import { getQuickKeys, subscribeQuickKeys } from '@/lib/quickKeysStorage'
import { useReducedMotion } from './use-reduced-motion'
import { layoutRing, type QuickKey } from '@/lib/quickKeys'
import { holdRepeatProps, useRepeatWhilePressed } from './use-repeat-while-pressed'

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

export default function QuickKeysMenu({ open, onSend, onClose, onCustomize }: Props) {
  const keys = useSyncExternalStore(subscribeQuickKeys, getQuickKeys)
  const reducedMotion = useReducedMotion()
  const repeat = useRepeatWhilePressed()
  // Items mount collapsed at the pad center and slide out on the next
  // frame so the CSS transform transition actually runs.
  const [armed, setArmed] = useState(false)
  const anchorRef = useRef<HTMLDivElement>(null)
  const [maxRadius, setMaxRadius] = useState(Number.POSITIVE_INFINITY)
  useEffect(() => {
    if (!open) return
    const raf = requestAnimationFrame(() => setArmed(true))
    return () => cancelAnimationFrame(raf)
  }, [open])

  // Concentric half rings grow outward from the pad; clamp how far the
  // outermost one may reach given the space around the pad center.
  useLayoutEffect(() => {
    const el = anchorRef.current
    if (!el) return
    const { left, top } = el.getBoundingClientRect()
    const freeSpace = Math.min(
      top - EDGE_MARGIN_PX,
      left - EDGE_MARGIN_PX,
      window.innerHeight - top - EDGE_MARGIN_PX
    )
    setMaxRadius(Math.max(48, freeSpace - ITEM_HALF_PX))
  }, [open])

  if (!open) return null

  const stagger = reducedMotion ? 0 : STAGGER_MS
  const duration = reducedMotion ? 0 : ENTER_DURATION_MS
  const { positions, customize } = layoutRing(keys.length, maxRadius)

  const popStyle = (dx: number, dy: number, index: number): CSSProperties => ({
    transform: armed
      ? `translate(calc(-50% + ${dx}px), calc(-50% + ${dy}px)) scale(1)`
      : 'translate(-50%, -50%) scale(0.25)',
    opacity: armed ? 1 : 0,
    transitionDuration: `${duration}ms`,
    transitionTimingFunction: SPRING_EASE,
    transitionDelay: `${index * stagger}ms`,
  })

  const labelStyle = (color?: string): CSSProperties | undefined => (color ? { color } : undefined)

  return (
    // The rings are centered on the pad button (wrapper center); the root
    // itself ignores pointers so the pad underneath stays draggable.
    <div ref={anchorRef} className="pointer-events-none absolute left-1/2 top-1/2">
      {keys.map((key, index) => (
        <button
          key={key.id}
          type="button"
          aria-label={`Send ${key.label}`}
          // `data` is the authoritative byte sequence: the editor stores it
          // next to the combo, so re-encoding here would silently rewrite what
          // keys saved by an older build send (their literal spaces included).
          {...holdRepeatProps(repeat, () => onSend(key))}
          style={{
            ...popStyle(positions[index].dx, positions[index].dy, index),
            ...labelStyle(key.color),
          }}
          className={cn(
            armed ? 'pointer-events-auto' : 'pointer-events-none',
            'absolute left-1/2 top-1/2 flex h-11 w-11 touch-none select-none',
            'items-center justify-center rounded-full border border-[hsl(var(--border))]',
            'bg-[hsl(var(--card))]/95 font-mono text-[10px] font-semibold leading-none',
            'text-[hsl(var(--foreground))] shadow-xl backdrop-blur-sm',
            'transition-[transform,opacity] active:opacity-70'
          )}
        >
          {key.label}
        </button>
      ))}
      <button
        type="button"
        aria-label="Customize quick keys"
        onClick={(event) => {
          event.stopPropagation()
          onCustomize()
          onClose()
        }}
        style={popStyle(customize.dx, customize.dy, keys.length)}
        className={cn(
          armed ? 'pointer-events-auto' : 'pointer-events-none',
          'absolute left-1/2 top-1/2 flex h-11 w-11 touch-none select-none',
          'items-center justify-center rounded-full border border-dashed border-[hsl(var(--border))]',
          'bg-[hsl(var(--muted))]/95 text-[hsl(var(--muted-foreground))] shadow-xl backdrop-blur-sm',
          'transition-[transform,opacity] active:opacity-70'
        )}
      >
        <SlidersHorizontal className="h-4 w-4" />
      </button>
    </div>
  )
}
