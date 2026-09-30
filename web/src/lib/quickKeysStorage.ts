/**
 * Persistence + tiny observable store for terminal quick keys.
 *
 * The radial menu and the editor dialog talk to this module only; the raw
 * localStorage shape is validated on read so a corrupted entry silently
 * falls back to the defaults instead of breaking the terminal.
 */
import { DEFAULT_QUICK_KEYS, type QuickKey } from '@/components/terminal/quick-keys'

const STORAGE_KEY = 'oly.terminal.quickKeys.v1'

export interface StoragePort {
  getItem(key: string): string | null
  setItem(key: string, value: string): void
}

function isQuickKey(value: unknown): value is QuickKey {
  if (typeof value !== 'object' || value === null) return false
  const record = value as Record<string, unknown>
  const colorOk =
    record.color === undefined ||
    (typeof record.color === 'string' && /^#[0-9a-fA-F]{3,8}$/.test(record.color))
  return (
    typeof record.id === 'string' &&
    record.id !== '' &&
    typeof record.label === 'string' &&
    record.label !== '' &&
    typeof record.data === 'string' &&
    record.data !== '' &&
    typeof record.combo === 'string' &&
    colorOk
  )
}

export function normalizeQuickKeys(raw: unknown): QuickKey[] | null {
  if (!Array.isArray(raw)) return null
  const keys: QuickKey[] = []
  const seen = new Set<string>()
  for (const entry of raw) {
    if (!isQuickKey(entry)) return null
    if (seen.has(entry.id)) return null
    seen.add(entry.id)
    const key: QuickKey = { id: entry.id, label: entry.label, data: entry.data, combo: entry.combo }
    if (entry.color !== undefined) key.color = entry.color
    keys.push(key)
  }
  return keys
}

export function loadQuickKeys(port?: StoragePort): QuickKey[] {
  try {
    const storage = port ?? globalThis.localStorage
    if (!storage) return DEFAULT_QUICK_KEYS
    const stored = storage.getItem(STORAGE_KEY)
    if (!stored) return DEFAULT_QUICK_KEYS
    const parsed = normalizeQuickKeys(JSON.parse(stored))
    return parsed ?? DEFAULT_QUICK_KEYS
  } catch {
    return DEFAULT_QUICK_KEYS
  }
}

export function saveQuickKeys(keys: QuickKey[], port?: StoragePort): void {
  try {
    const storage = port ?? globalThis.localStorage
    storage?.setItem(STORAGE_KEY, JSON.stringify(keys))
  } catch {
    // Storage unavailable (private mode, PWA quota): keep the session-only
    // value in the in-memory cache below.
  }
}

type Listener = () => void

let cache: QuickKey[] | null = null
const listeners = new Set<Listener>()

function notify(): void {
  for (const listener of listeners) listener()
}

export function subscribeQuickKeys(listener: Listener): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

export function getQuickKeys(): QuickKey[] {
  if (!cache) cache = loadQuickKeys()
  return cache
}

export function setQuickKeys(keys: QuickKey[]): void {
  cache = keys
  saveQuickKeys(keys)
  notify()
}

export function resetQuickKeys(): void {
  setQuickKeys(DEFAULT_QUICK_KEYS)
}
