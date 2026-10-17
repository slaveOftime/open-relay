/**
 * The header of one group in the sessions list, and its collapse toggle.
 *
 * The whole header is one button: pressing anywhere on it hides the group's
 * cards/rows or brings them back, and the caret turns to point at the label
 * while they are hidden. The two layouts (mobile cards, desktop table) render
 * the same header inside their own sticky, opaque wrapper — the wrapper has to
 * differ because a table row is not a div, but everything interactive and
 * visible here is shared, so the two can never drift apart.
 */

import { CaretDownIcon } from '@radix-ui/react-icons'

import type { SessionSummary } from '@/api/types'
import { cn } from '@/utils/cn'
import { GroupHeaderLabel } from './GroupHeaderLabel'
import type { GroupBy } from './group-by'

export function SessionGroupHeader({
  groupBy,
  keyLabel,
  items,
  collapsed,
  onToggle,
}: {
  groupBy: GroupBy
  keyLabel: string
  items: SessionSummary[]
  collapsed: boolean
  onToggle: () => void
}) {
  return (
    <button
      type="button"
      aria-expanded={!collapsed}
      onClick={onToggle}
      className="flex w-full cursor-pointer items-center gap-1 bg-[hsl(var(--primary))]/10 px-2 py-1 text-left text-xs font-medium text-[hsl(var(--muted-foreground))] focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-[hsl(var(--ring))] focus-visible:outline-none"
    >
      <CaretDownIcon
        aria-hidden="true"
        className={cn('h-4 w-4 shrink-0 transition-transform', collapsed && '-rotate-90')}
      />
      <GroupHeaderLabel groupBy={groupBy} keyLabel={keyLabel} items={items} />
    </button>
  )
}
