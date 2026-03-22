import type { NodeSummary } from '@/api/types.ts'
import { cn } from '@/lib/utils'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select.tsx'

interface NodeSelectorProps {
  nodes: NodeSummary[]
  selected: string | null
  onChange: (node: string | null) => void
  className?: string
}

export function NodeSelector({ nodes, selected, onChange, className }: NodeSelectorProps) {
  if (nodes.length === 0 && selected === null) return null

  return (
    <Select
      value={selected ?? '__local__'}
      onValueChange={(v) => onChange(v === '__local__' ? null : v)}
    >
      <SelectTrigger className={cn('flex-1 sm:flex-0 h-8 text-xs uppercase', className)}>
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        <SelectItem value="__local__" className="text-xs uppercase">
          Local
        </SelectItem>
        {nodes.map((n) => (
          <SelectItem key={n.name} value={n.name} className="text-xs">
            {n.name}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  )
}
