import { useRef, useState, useSyncExternalStore } from 'react'
import { GripVertical, Plus, RotateCcw, X } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { cn } from '@/lib/utils'
import { getQuickKeys, resetQuickKeys, setQuickKeys, subscribeQuickKeys } from '@/lib/quickKeysStorage'
import { describeData, encodeCombo, formatCombo, moveQuickKey, type QuickKey } from './quick-keys'

/** Fixed row height (h-14) + vertical margin (mb-2) used for drag math. */
const ROW_STRIDE_PX = 64

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
  const dragRef = useRef<DragState | null>(null)
  const [dragView, setDragView] = useState<{ id: string; delta: number } | null>(null)

  const preview = combo.trim() ? encodeCombo(combo.trim()) ?? combo.trim() : ''

  function handleAdd() {
    const trimmed = combo.trim()
    if (!trimmed) return
    const data = encodeCombo(trimmed) ?? trimmed
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

  function handleRemove(id: string) {
    setQuickKeys(getQuickKeys().filter((key) => key.id !== id))
  }

  function handleDragStart(event: React.PointerEvent<HTMLElement>, index: number, id: string) {
    event.preventDefault()
    event.currentTarget.setPointerCapture(event.pointerId)
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
      <DialogContent className="max-w-sm">
        <DialogHeader>
          <DialogTitle>Quick Keys</DialogTitle>
          <DialogDescription>
            Tap to send · drag the handle to reorder. Keys are sent to the terminal as soon as the
            round button is tapped.
          </DialogDescription>
        </DialogHeader>

        <div role="list" aria-label="Quick keys" className="-mx-1 max-h-[45vh] overflow-y-auto">
          {keys.length === 0 ? (
            <p className="py-3 text-center text-sm text-[hsl(var(--muted-foreground))]">
              No quick keys. Add one below or restore the defaults.
            </p>
          ) : null}
          {keys.map((key, index) => {
            const dragging = dragView?.id === key.id
            return (
              <div
                key={key.id}
                role="listitem"
                style={
                  dragging
                    ? { transform: `translateY(${dragView?.delta ?? 0}px)`, zIndex: 10 }
                    : undefined
                }
                className={cn(
                  'relative mb-2 flex h-14 items-center gap-2 rounded-lg border px-2',
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
                <span className="flex h-11 w-11 shrink-0 items-center justify-center rounded-full border border-[hsl(var(--border))] bg-[hsl(var(--muted))] font-mono text-[10px] font-semibold leading-none">
                  {key.label}
                </span>
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
              placeholder="Combo, e.g. ctrl+c or up"
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
              {preview ? `Sends: ${describeData(preview)}` : 'e.g. ctrl+c, shift+tab, alt+f, or any text'}
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
