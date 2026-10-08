import { useRef, useState, useSyncExternalStore } from 'react'
import { GripVertical, Plus, RotateCcw, X } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { cn } from '@/lib/utils'
import {
  getQuickKeys,
  resetQuickKeys,
  setQuickKeys,
  subscribeQuickKeys,
} from '@/lib/quickKeysStorage'
import {
  describeData,
  encodeQuickKeyCombo,
  formatCombo,
  moveQuickKey,
  QUICK_KEY_COLORS,
  type QuickKey,
} from './quick-keys'

/** Fixed row height (h-14) + vertical margin (mb-2) used for drag math. */
const ROW_STRIDE_PX = 64

function Swatch({
  color,
  selected,
  onSelect,
  label,
}: {
  color: string | undefined
  selected: boolean
  onSelect: (color: string | undefined) => void
  label: string
}) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      aria-label={label}
      onClick={() => onSelect(color)}
      style={color ? { color } : undefined}
      className={cn(
        'flex h-8 w-8 shrink-0 items-center justify-center rounded-full border font-mono text-[11px] font-bold leading-none',
        selected
          ? 'border-[hsl(var(--ring))] bg-[hsl(var(--accent))]'
          : 'border-[hsl(var(--border))] bg-[hsl(var(--muted))]'
      )}
    >
      A
    </button>
  )
}

interface DragState {
  id: string
  /** Index the dragged row currently occupies. */
  index: number
  /** Pointer Y whose delta against the row origin is zero. */
  originY: number
}

interface Props {
  open: boolean
  onOpenChange: (open: boolean) => void
}

export default function QuickKeysDialog({ open, onOpenChange }: Props) {
  const keys = useSyncExternalStore(subscribeQuickKeys, getQuickKeys)
  const [combo, setCombo] = useState('')
  const [label, setLabel] = useState('')
  const [paletteFor, setPaletteFor] = useState<string | null>(null)
  const dragRef = useRef<DragState | null>(null)
  const [dragView, setDragView] = useState<{ id: string; delta: number } | null>(null)

  const preview = combo.trim() ? (encodeQuickKeyCombo(combo.trim()) ?? combo.trim()) : ''

  function handleAdd() {
    const trimmed = combo.trim()
    if (!trimmed) return
    const data = encodeQuickKeyCombo(trimmed) ?? trimmed
    const key: QuickKey = {
      id: `${trimmed.toLowerCase()}::${Date.now()}`,
      label: (label.trim() || formatCombo(trimmed)).slice(0, 4),
      data,
      combo: trimmed.toLowerCase(),
    }
    setQuickKeys([...getQuickKeys(), key])
    setCombo('')
    setLabel('')
  }

  function handleColorSet(id: string, color: string | undefined) {
    setQuickKeys(
      getQuickKeys().map((key) => {
        if (key.id !== id) return key
        if (color) return { ...key, color }
        const rest = { ...key }
        delete rest.color
        return rest
      })
    )
    setPaletteFor(null)
  }

  function handleRemove(id: string) {
    setQuickKeys(getQuickKeys().filter((key) => key.id !== id))
  }

  function handleDragStart(event: React.PointerEvent<HTMLElement>, index: number, id: string) {
    event.preventDefault()
    event.currentTarget.setPointerCapture(event.pointerId)
    setPaletteFor(null)
    dragRef.current = { id, index, originY: event.clientY }
    setDragView({ id, delta: 0 })
  }

  function handleDragMove(event: React.PointerEvent<HTMLElement>) {
    const drag = dragRef.current
    if (!drag) return
    const delta = event.clientY - drag.originY
    const shift = Math.round(delta / ROW_STRIDE_PX)
    if (shift !== 0) {
      const target = Math.min(Math.max(drag.index + shift, 0), getQuickKeys().length - 1)
      setQuickKeys(moveQuickKey(getQuickKeys(), drag.index, target))
      drag.index = target
      drag.originY += shift * ROW_STRIDE_PX
    }
    setDragView({ id: drag.id, delta: event.clientY - drag.originY })
  }

  function handleDragEnd(event: React.PointerEvent<HTMLElement>) {
    if (dragRef.current && event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId)
    }
    dragRef.current = null
    setDragView(null)
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex max-h-[calc(100dvh-2rem)] max-w-sm flex-col overflow-hidden">
        <DialogHeader>
          <DialogTitle>Quick Keys</DialogTitle>
          <DialogDescription>
            Tap to send · drag the handle to reorder · tap a label to recolor it. Keys are sent to
            the terminal as soon as the round button is tapped.
          </DialogDescription>
        </DialogHeader>

        <div role="list" aria-label="Quick keys" className="-mx-1 min-h-0 flex-1 overflow-y-auto">
          {keys.length === 0 ? (
            <p className="py-3 text-center text-sm text-[hsl(var(--muted-foreground))]">
              No quick keys. Add one below or restore the defaults.
            </p>
          ) : null}
          {keys.map((key, index) => {
            const dragging = dragView?.id === key.id
            const paletteOpen = paletteFor === key.id
            return (
              <div key={key.id} role="listitem" className="relative mb-2">
                <div
                  style={
                    dragging
                      ? { transform: `translateY(${dragView?.delta ?? 0}px)`, zIndex: 10 }
                      : undefined
                  }
                  className={cn(
                    'relative flex h-14 items-center gap-2 rounded-lg border px-2',
                    'border-[hsl(var(--border))] bg-[hsl(var(--background))]',
                    dragging && 'border-[hsl(var(--ring))] shadow-lg'
                  )}
                >
                  <button
                    type="button"
                    aria-label={`Reorder ${key.label}`}
                    onPointerDown={(event) => handleDragStart(event, index, key.id)}
                    onPointerMove={handleDragMove}
                    onPointerUp={handleDragEnd}
                    onPointerCancel={handleDragEnd}
                    className="flex h-11 w-11 shrink-0 cursor-grab touch-none items-center justify-center rounded-md text-[hsl(var(--muted-foreground))] active:cursor-grabbing"
                  >
                    <GripVertical className="h-4 w-4" />
                  </button>
                  <button
                    type="button"
                    aria-label={`Change color of ${key.label}`}
                    aria-expanded={paletteOpen}
                    onClick={() => setPaletteFor(paletteOpen ? null : key.id)}
                    style={key.color ? { color: key.color, borderColor: key.color } : undefined}
                    className={cn(
                      'flex h-11 w-11 shrink-0 items-center justify-center rounded-full border border-[hsl(var(--border))] bg-[hsl(var(--muted))] font-mono text-[10px] font-semibold leading-none transition-transform active:scale-90',
                      !key.color && 'text-[hsl(var(--foreground))]',
                      paletteOpen && 'border-[hsl(var(--ring))]'
                    )}
                  >
                    {key.label}
                  </button>
                  <span className="flex min-w-0 flex-col gap-0.5">
                    <span className="truncate text-xs font-medium">{formatCombo(key.combo)}</span>
                    <span className="truncate font-mono text-[11px] text-[hsl(var(--muted-foreground))]">
                      {describeData(key.data)}
                    </span>
                  </span>
                  <button
                    type="button"
                    aria-label={`Remove ${key.label}`}
                    onClick={() => handleRemove(key.id)}
                    className="ml-auto flex h-11 w-11 shrink-0 items-center justify-center rounded-md text-[hsl(var(--muted-foreground))] transition-colors hover:bg-[hsl(var(--muted))] hover:text-[hsl(var(--destructive))]"
                  >
                    <X className="h-4 w-4" />
                  </button>
                </div>
                {paletteOpen ? (
                  <div
                    className="flex flex-wrap items-center gap-1.5 p-2"
                    role="radiogroup"
                    aria-label={`Color of ${key.label}`}
                  >
                    <Swatch
                      color={undefined}
                      selected={!key.color}
                      onSelect={(next) => handleColorSet(key.id, next)}
                      label="Default color"
                    />
                    {QUICK_KEY_COLORS.map((option) => (
                      <Swatch
                        key={option}
                        color={option}
                        selected={key.color === option}
                        onSelect={(next) => handleColorSet(key.id, next)}
                        label={`Color ${option}`}
                      />
                    ))}
                  </div>
                ) : null}
              </div>
            )
          })}
        </div>

        <form
          className="flex flex-col gap-2 rounded-lg border border-dashed border-[hsl(var(--border))] p-3"
          onSubmit={(event) => {
            event.preventDefault()
            handleAdd()
          }}
        >
          <div className="flex gap-2">
            <Input
              value={combo}
              onChange={(event) => setCombo(event.target.value)}
              placeholder="Combo, e.g. ctrl+c or enter space"
              className="h-11 flex-1"
              aria-label="Key combo"
            />
            <Input
              value={label}
              onChange={(event) => setLabel(event.target.value)}
              placeholder="Label"
              className="h-11 w-20"
              aria-label="Button label"
              maxLength={4}
            />
          </div>
          <div className="flex items-center justify-between gap-2">
            <span className="min-w-0 truncate font-mono text-[11px] text-[hsl(var(--muted-foreground))]">
              {preview
                ? `Sends: ${describeData(preview).replaceAll(' ', '␣')}`
                : 'e.g. ctrl+c, shift+tab, alt+f, text, enter, or space'}
            </span>
            <Button type="submit" size="sm" variant="outline" disabled={!combo.trim()}>
              <Plus className="h-4 w-4" />
              Add
            </Button>
          </div>
        </form>

        <div className="flex justify-end">
          <Button type="button" size="sm" variant="ghost" onClick={() => resetQuickKeys()}>
            <RotateCcw className="h-4 w-4" />
            Restore defaults
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  )
}
