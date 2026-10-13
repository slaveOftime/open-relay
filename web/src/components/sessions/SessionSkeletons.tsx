/**
 * Loading placeholders. The desktop table and the mobile card list are two
 * different layouts, so each gets its own skeleton rather than one shape
 * squeezed into both.
 */

import { TableCell } from '@/components/ui/table'

export function SkeletonRow() {
  return (
    <tr className="border-b border-[hsl(var(--border))]">
      {[8, 10, 24, 14, 22, 30, 12, 16, 10, 8, 18].map((w, i) => (
        <TableCell key={i} className="px-3 py-3">
          <div
            className="h-3 rounded animate-shimmer"
            style={{ width: `${w + ((i * 7) % 10)}%` }}
          />
        </TableCell>
      ))}
    </tr>
  )
}

export function SkeletonCard() {
  return (
    <div className="mx-3 my-2 rounded-xl border border-[hsl(var(--border))] bg-[hsl(var(--card))] p-4 flex flex-col gap-3">
      <div className="flex items-center justify-between">
        <div className="h-4 w-20 rounded-full animate-shimmer" />
        <div className="h-3 w-10 rounded animate-shimmer" />
      </div>
      <div className="h-3.5 rounded animate-shimmer" style={{ width: '60%' }} />
      <div className="flex gap-2">
        <div className="h-3 w-14 rounded animate-shimmer" />
        <div className="h-3 w-12 rounded animate-shimmer" />
      </div>
    </div>
  )
}
