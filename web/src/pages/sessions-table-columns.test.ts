import { describe, expect, it } from 'vitest'

import {
  clampSessionTableColumnSize,
  coerceSessionTableColumnOrder,
  coerceSessionTableColumnSettings,
  coerceSessionTableColumnSizes,
  getOrderedSessionTableColumns,
  getSessionTableWidth,
  reorderSessionTableColumn,
  SESSION_TABLE_COLUMNS,
} from './sessions-table-columns'

describe('sessions table columns', () => {
  it('coerces missing persisted column widths to defaults', () => {
    const sizes = coerceSessionTableColumnSizes({ id: 120 })

    expect(sizes.id).toBe(120)
    expect(sizes.title).toBe(SESSION_TABLE_COLUMNS.find((column) => column.key === 'title')?.defaultWidth)
  })

  it('clamps unsafe persisted column widths', () => {
    expect(clampSessionTableColumnSize('id', 1)).toBe(64)
    expect(clampSessionTableColumnSize('cwd', 900)).toBe(640)
  })

  it('sums coerced widths for the table minimum width', () => {
    const sizes = coerceSessionTableColumnSizes({ id: 100, pid: 100 })

    expect(getSessionTableWidth(sizes)).toBe(
      SESSION_TABLE_COLUMNS.reduce((total, column) => total + sizes[column.key], 0)
    )
  })

  it('coerces persisted column order and appends missing defaults', () => {
    expect(coerceSessionTableColumnOrder(['title', 'id', 'title', 'unknown'])).toEqual([
      'title',
      'id',
      ...SESSION_TABLE_COLUMNS.map((column) => column.key).filter(
        (key) => key !== 'title' && key !== 'id'
      ),
    ])
  })

  it('coerces combined settings while preserving legacy size-only storage', () => {
    const legacy = coerceSessionTableColumnSettings({ id: 120 })
    expect(legacy.sizes.id).toBe(120)
    expect(legacy.order[0]).toBe('id')

    const combined = coerceSessionTableColumnSettings({
      sizes: { id: 140 },
      order: ['title', 'id'],
    })
    expect(combined.sizes.id).toBe(140)
    expect(combined.order.slice(0, 2)).toEqual(['title', 'id'])
  })

  it('returns column metadata in persisted order', () => {
    const columns = getOrderedSessionTableColumns(['title', 'id'])

    expect(columns[0]?.key).toBe('title')
    expect(columns[1]?.key).toBe('id')
  })

  it('moves a dragged column before the drop target', () => {
    expect(reorderSessionTableColumn(['id', 'title', 'pid'], 'pid', 'title').slice(0, 3)).toEqual([
      'id',
      'pid',
      'title',
    ])
  })
})
