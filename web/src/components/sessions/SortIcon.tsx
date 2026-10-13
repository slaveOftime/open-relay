/**
 * Sort affordance for a table column header: a neutral caret unless the
 * column is the active sort, then the matching direction glyph.
 */

import { CaretSortIcon, ChevronDownIcon, ChevronUpIcon } from '@radix-ui/react-icons'
import { SortOrder, type SessionSortField } from '@/api/types'

export function SortIcon({
  field,
  sortField,
  sortOrder,
}: {
  field: SessionSortField
  sortField: SessionSortField
  sortOrder: SortOrder
}) {
  if (field !== sortField) return <CaretSortIcon className="w-3 h-3 opacity-40" />
  return sortOrder === SortOrder.Asc ? (
    <ChevronUpIcon className="w-3 h-3" />
  ) : (
    <ChevronDownIcon className="w-3 h-3" />
  )
}
