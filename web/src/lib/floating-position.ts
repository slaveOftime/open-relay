/**
 * Persistence + geometry for the two hold-then-drag floating controls (the
 * terminal's scroll handle, the sessions search trigger).
 *
 * The stored value is an offset from the container's top-left corner, clamped
 * on load, on save and whenever the container resizes — so a position saved on
 * a big window cannot strand the button off the edge of a small one. The raw
 * localStorage shape is validated on read: a corrupted entry falls back to the
 * CSS default instead of parking the button somewhere silly.
 */

export interface StoragePort {
  getItem(key: string): string | null
  setItem(key: string, value: string): void
}

/** Offset of the element from its container's top-left corner, in pixels. */
export interface FloatPosition {
  x: number
  y: number
}

export interface Size {
  width: number
  height: number
}

/** Keep a floating control this far from every container edge. */
export const EDGE_MARGIN_PX = 4

/** Validate a parsed entry: an object of two finite numbers, or nothing. */
export function normalizePosition(raw: unknown): FloatPosition | null {
  if (typeof raw !== 'object' || raw === null) return null
  const record = raw as Record<string, unknown>
  const { x, y } = record
  if (typeof x !== 'number' || typeof y !== 'number') return null
  if (!Number.isFinite(x) || !Number.isFinite(y)) return null
  return { x, y }
}

export function loadPosition(key: string, port?: StoragePort): FloatPosition | null {
  try {
    const storage = port ?? globalThis.localStorage
    if (!storage) return null
    const stored = storage.getItem(key)
    if (!stored) return null
    return normalizePosition(JSON.parse(stored))
  } catch {
    return null
  }
}

export function savePosition(key: string, position: FloatPosition, port?: StoragePort): void {
  try {
    const storage = port ?? globalThis.localStorage
    storage?.setItem(key, JSON.stringify(position))
  } catch {
    // Storage unavailable (private mode, PWA quota): the button keeps its new
    // spot for this session and goes back to the default on the next load.
  }
}

/**
 * Clamp a position so the element stays fully inside the container, `margin`
 * pixels from each edge. When the container is smaller than the element plus
 * both margins, the element is pinned at the margin rather than allowed a
 * negative offset (there is nowhere better for it to go).
 */
export function clampPosition(
  position: FloatPosition,
  element: Size,
  container: Size,
  margin: number = EDGE_MARGIN_PX
): FloatPosition {
  const maxX = Math.max(margin, container.width - element.width - margin)
  const maxY = Math.max(margin, container.height - element.height - margin)
  return {
    x: Math.min(Math.max(position.x, margin), maxX),
    y: Math.min(Math.max(position.y, margin), maxY),
  }
}
