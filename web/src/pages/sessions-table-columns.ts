import { SessionSortField } from '@/api/types'

export const SESSION_TABLE_COLUMN_STORAGE_KEY = 'open-relay.webv2.sessions.table-columns.v1'

export const SESSION_TABLE_COLUMNS = [
  { key: 'id', label: 'ID', sortField: SessionSortField.Id, defaultWidth: 80, minWidth: 64 },
  { key: 'output', label: 'Output', sortField: undefined, defaultWidth: 96, minWidth: 72 },
  { key: 'title', label: 'Title', sortField: SessionSortField.Title, defaultWidth: 176, minWidth: 96 },
  { key: 'tags', label: 'Tags', sortField: undefined, defaultWidth: 152, minWidth: 96 },
  {
    key: 'command',
    label: 'Command',
    sortField: SessionSortField.Command,
    defaultWidth: 192,
    minWidth: 120,
  },
  { key: 'cwd', label: 'CWD', sortField: SessionSortField.Cwd, defaultWidth: 240, minWidth: 120 },
  {
    key: 'status',
    label: 'Status',
    sortField: SessionSortField.Status,
    defaultWidth: 120,
    minWidth: 96,
  },
  {
    key: 'created_at',
    label: 'Created At',
    sortField: SessionSortField.CreatedAt,
    defaultWidth: 160,
    minWidth: 128,
  },
  { key: 'activity', label: 'Activity', sortField: undefined, defaultWidth: 104, minWidth: 80 },
  { key: 'pid', label: 'PID', sortField: SessionSortField.Pid, defaultWidth: 72, minWidth: 56 },
  { key: 'actions', label: 'Actions', sortField: undefined, defaultWidth: 168, minWidth: 136 },
] as const

export type SessionTableColumn = (typeof SESSION_TABLE_COLUMNS)[number]
export type SessionTableColumnKey = SessionTableColumn['key']
export type SessionTableColumnSizes = Record<SessionTableColumnKey, number>
export type SessionTableColumnOrder = SessionTableColumnKey[]
export type SessionTableColumnSettings = {
  sizes: SessionTableColumnSizes
  order: SessionTableColumnOrder
}

const MAX_COLUMN_WIDTH = 640

function defaultSessionTableColumnSizes(): SessionTableColumnSizes {
  return SESSION_TABLE_COLUMNS.reduce((sizes, column) => {
    sizes[column.key] = column.defaultWidth
    return sizes
  }, {} as SessionTableColumnSizes)
}

function defaultSessionTableColumnOrder(): SessionTableColumnOrder {
  return SESSION_TABLE_COLUMNS.map((column) => column.key)
}

export function clampSessionTableColumnSize(
  columnKey: SessionTableColumnKey,
  width: number
): number {
  const column = SESSION_TABLE_COLUMNS.find((item) => item.key === columnKey)
  if (!column || !Number.isFinite(width)) {
    return defaultSessionTableColumnSizes()[columnKey]
  }

  return Math.min(MAX_COLUMN_WIDTH, Math.max(column.minWidth, Math.round(width)))
}

export function coerceSessionTableColumnSizes(raw: unknown): SessionTableColumnSizes {
  const defaults = defaultSessionTableColumnSizes()
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) {
    return defaults
  }

  const record = raw as Partial<Record<SessionTableColumnKey, unknown>>
  return SESSION_TABLE_COLUMNS.reduce((sizes, column) => {
    const value = record[column.key]
    sizes[column.key] =
      typeof value === 'number' ? clampSessionTableColumnSize(column.key, value) : defaults[column.key]
    return sizes
  }, {} as SessionTableColumnSizes)
}

export function coerceSessionTableColumnOrder(raw: unknown): SessionTableColumnOrder {
  const defaults = defaultSessionTableColumnOrder()
  if (!Array.isArray(raw)) return defaults

  const validKeys = new Set(defaults)
  const seenKeys = new Set<SessionTableColumnKey>()
  const order: SessionTableColumnOrder = []
  for (const value of raw) {
    if (typeof value !== 'string' || !validKeys.has(value as SessionTableColumnKey)) continue
    const key = value as SessionTableColumnKey
    if (seenKeys.has(key)) continue
    seenKeys.add(key)
    order.push(key)
  }

  for (const key of defaults) {
    if (!seenKeys.has(key)) order.push(key)
  }
  return order
}

export function coerceSessionTableColumnSettings(raw: unknown): SessionTableColumnSettings {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) {
    return {
      sizes: coerceSessionTableColumnSizes(null),
      order: coerceSessionTableColumnOrder(null),
    }
  }

  const record = raw as { sizes?: unknown; order?: unknown }
  return {
    sizes: coerceSessionTableColumnSizes(record.sizes ?? raw),
    order: coerceSessionTableColumnOrder(record.order),
  }
}

export function getOrderedSessionTableColumns(order: SessionTableColumnOrder): SessionTableColumn[] {
  const columnsByKey = new Map(SESSION_TABLE_COLUMNS.map((column) => [column.key, column]))
  return coerceSessionTableColumnOrder(order).flatMap((key) => {
    const column = columnsByKey.get(key)
    return column ? [column] : []
  })
}

export function reorderSessionTableColumn(
  order: SessionTableColumnOrder,
  sourceKey: SessionTableColumnKey,
  targetKey: SessionTableColumnKey
): SessionTableColumnOrder {
  if (sourceKey === targetKey) return order

  const nextOrder = coerceSessionTableColumnOrder(order)
  const sourceIndex = nextOrder.indexOf(sourceKey)
  const targetIndex = nextOrder.indexOf(targetKey)
  if (sourceIndex === -1 || targetIndex === -1) return nextOrder

  nextOrder.splice(sourceIndex, 1)
  nextOrder.splice(targetIndex, 0, sourceKey)
  return nextOrder
}

export function getSessionTableWidth(sizes: SessionTableColumnSizes): number {
  return SESSION_TABLE_COLUMNS.reduce((total, column) => total + sizes[column.key], 0)
}
