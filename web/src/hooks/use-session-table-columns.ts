/**
 * Column resize and reorder gestures for the sessions table.
 *
 * Resize is pointer-capture based: the handle holds the pointer from pointerdown
 * to pointerup, so a drag that leaves the handle still tracks. Reorder uses the
 * native drag events on the header cells.
 *
 * The hook owns only the two "which gesture is running" refs; the settings and
 * their persistence belong to `pages/sessions-page-prefs.ts`.
 */

import { useCallback, useRef } from 'react'

import {
  clampSessionTableColumnSize,
  reorderSessionTableColumn,
  type SessionTableColumnKey,
  type SessionTableColumnOrder,
  type SessionTableColumnSizes,
} from '@/lib/sessions-table-columns'

export interface SessionTableColumnSettings {
  sizes: SessionTableColumnSizes
  order: SessionTableColumnOrder
}

interface ActiveResize {
  columnKey: SessionTableColumnKey
  startX: number
  startWidth: number
}

export interface SessionTableColumnGestures {
  beginResize: (columnKey: SessionTableColumnKey, event: React.PointerEvent) => void
  updateResize: (event: React.PointerEvent) => void
  endResize: (event: React.PointerEvent) => void
  beginReorder: (columnKey: SessionTableColumnKey, event: React.DragEvent) => void
  moveBefore: (columnKey: SessionTableColumnKey, event: React.DragEvent) => void
  dropBefore: (columnKey: SessionTableColumnKey, event: React.DragEvent) => void
  endReorder: () => void
}

export function useSessionTableColumns(
  settings: SessionTableColumnSettings,
  setSettings: React.Dispatch<React.SetStateAction<SessionTableColumnSettings>>
): SessionTableColumnGestures {
  const resizeRef = useRef<ActiveResize | null>(null)
  const dragRef = useRef<SessionTableColumnKey | null>(null)

  const beginResize = useCallback(
    (columnKey: SessionTableColumnKey, event: React.PointerEvent) => {
      event.preventDefault()
      event.stopPropagation()
      resizeRef.current = {
        columnKey,
        startX: event.clientX,
        startWidth: settings.sizes[columnKey],
      }
      event.currentTarget.setPointerCapture(event.pointerId)
    },
    [settings.sizes]
  )

  const updateResize = useCallback(
    (event: React.PointerEvent) => {
      const resize = resizeRef.current
      if (!resize) return
      event.preventDefault()
      const nextWidth = clampSessionTableColumnSize(
        resize.columnKey,
        resize.startWidth + event.clientX - resize.startX
      )
      setSettings((previous) => {
        if (previous.sizes[resize.columnKey] === nextWidth) return previous
        return {
          ...previous,
          sizes: {
            ...previous.sizes,
            [resize.columnKey]: nextWidth,
          },
        }
      })
    },
    [setSettings]
  )

  const endResize = useCallback((event: React.PointerEvent) => {
    if (!resizeRef.current) return
    resizeRef.current = null
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId)
    }
  }, [])

  const beginReorder = useCallback((columnKey: SessionTableColumnKey, event: React.DragEvent) => {
    dragRef.current = columnKey
    event.dataTransfer.effectAllowed = 'move'
    event.dataTransfer.setData('text/plain', columnKey)
  }, [])

  const moveBefore = useCallback((columnKey: SessionTableColumnKey, event: React.DragEvent) => {
    const draggedColumn = dragRef.current
    if (!draggedColumn || draggedColumn === columnKey) return
    event.preventDefault()
    event.dataTransfer.dropEffect = 'move'
  }, [])

  const dropBefore = useCallback(
    (columnKey: SessionTableColumnKey, event: React.DragEvent) => {
      const draggedColumn =
        dragRef.current || (event.dataTransfer.getData('text/plain') as SessionTableColumnKey)
      dragRef.current = null
      if (!draggedColumn || draggedColumn === columnKey) return
      event.preventDefault()
      setSettings((previous) => ({
        ...previous,
        order: reorderSessionTableColumn(previous.order, draggedColumn, columnKey),
      }))
    },
    [setSettings]
  )

  const endReorder = useCallback(() => {
    dragRef.current = null
  }, [])

  return {
    beginResize,
    updateResize,
    endResize,
    beginReorder,
    moveBefore,
    dropBefore,
    endReorder,
  }
}
