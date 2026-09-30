import { useEffect, useState, useSyncExternalStore, type CSSProperties } from 'react'
import { SlidersHorizontal } from 'lucide-react'
import { cn } from '@/lib/utils'
import { getQuickKeys, subscribeQuickKeys } from '@/lib/quickKeysStorage'
import { useReducedMotion } from './use-reduced-motion'
import type { QuickKey } from './quick-keys'

const BASE_RADIUS_PX = 76
/** Minimum center-to-center distance so adjacent buttons stay 44px targets. */
const MIN_SPACING_PX = 52
const CUSTOMIZE_OFFSET_PX = 104
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

function ringPosition(index: number, count: number, radius: number) {
  const angle = ((index / Math.max(count, 1)) * Math.PI * 2) % (Math.PI * 2)
  return {
    dx: Math.cos(angle - Math.PI / 2) * radius,
    dy: Math.sin(angle - Math.PI / 2) * radius,
  }
}

export default function QuickKeysMenu({ open, onSend, onClose, onCustomize }: Props) {
  const keys = useSyncExternalStore(subscribeQuickKeys, getQuickKeys)
  const reducedMotion = useReducedMotion()
  // Items mount collapsed at the pad center and slide out on the next
  // frame so the CSS transform transition actually runs.
  const [armed, setArmed] = useState(false)
  useEffect(() => {
    if (!open) return
    const raf = requestAnimationFrame(() => setArmed(true))
    return () => cancelAnimationFrame(raf)
  }, [open])

  if (!open) return null

  const wantedRadius = Math.max(BASE_RADIUS_PX, (keys.length * MIN_SPACING_PX) / (Math.PI * 2))
  // Never let the ring grow past what fits on screen around the dock point.
  const maxRadius = Math.min(window.innerWidth, window.innerHeight) / 2 - 60
  const radius = Math.max(BASE_RADIUS_PX, Math.min(wantedRadius, maxRadius))
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

  return (
    // The ring is centered on the main pad button (wrapper center); the root
    // itself ignores pointers so the pad underneath stays draggable.
    <div className="pointer-events-none absolute left-1/2 top-1/2">
      {keys.map((key, index) => {
        const { dx, dy } = ringPosition(index, keys.length, radius)
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
        style={popStyle(0, CUSTOMIZE_OFFSET_PX, keys.length)}
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
