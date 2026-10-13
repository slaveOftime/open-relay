/**
 * Per-session persistence for the attach panel: the input draft, whether the
 * drawer was open, and the image-preview cache.
 *
 * These keys are namespaced by session id. Anything unavailable (private
 * mode, quota exhausted, no `localStorage` at all) degrades to the same
 * "not stored" answer as a missing key, so the panel keeps working.
 */

import type { SessionImagePreviews } from './attach-panel-image-preview'
import { coerceSessionImagePreviews } from './attach-panel-image-preview'

// ── Per-session keys ─────────────────────────────────────────────────────────
const SESSION_INPUT_DRAFT_KEY_PREFIX = 'open-relay:session-input-draft:'
const SESSION_DRAWER_OPEN_KEY_PREFIX = 'open-relay:session-drawer-open:'
const SESSION_IMAGE_PREVIEW_KEY_PREFIX = 'open-relay:session-image-preview:'

function getSessionInputDraftKey(sessionId: string): string | null {
  const trimmed = sessionId.trim()
  return trimmed ? `${SESSION_INPUT_DRAFT_KEY_PREFIX}${trimmed}` : null
}

export function loadSessionInputDraft(sessionId: string): string {
  const storageKey = getSessionInputDraftKey(sessionId)
  if (!storageKey) return ''
  try {
    return localStorage.getItem(storageKey) ?? ''
  } catch {
    return ''
  }
}

export function saveSessionInputDraft(sessionId: string, text: string): void {
  const storageKey = getSessionInputDraftKey(sessionId)
  if (!storageKey) return
  try {
    if (text.length === 0) {
      localStorage.removeItem(storageKey)
      return
    }
    localStorage.setItem(storageKey, text)
  } catch {
    /* ignore */
  }
}

function getSessionDrawerOpenKey(sessionId: string): string | null {
  const trimmed = sessionId.trim()
  return trimmed ? `${SESSION_DRAWER_OPEN_KEY_PREFIX}${trimmed}` : null
}

export function loadSessionDrawerOpen(sessionId: string): boolean {
  const storageKey = getSessionDrawerOpenKey(sessionId)
  if (!storageKey) return false
  try {
    return localStorage.getItem(storageKey) === '1'
  } catch {
    return false
  }
}

export function saveSessionDrawerOpen(sessionId: string, isOpen: boolean): void {
  const storageKey = getSessionDrawerOpenKey(sessionId)
  if (!storageKey) return
  try {
    if (!isOpen) {
      localStorage.removeItem(storageKey)
      return
    }
    localStorage.setItem(storageKey, '1')
  } catch {
    /* ignore */
  }
}

function getSessionImagePreviewKey(sessionId: string): string | null {
  const trimmed = sessionId.trim()
  return trimmed ? `${SESSION_IMAGE_PREVIEW_KEY_PREFIX}${trimmed}` : null
}

export function loadSessionImagePreviews(sessionId: string): SessionImagePreviews {
  const storageKey = getSessionImagePreviewKey(sessionId)
  if (!storageKey) return {}
  try {
    const raw = sessionStorage.getItem(storageKey)
    if (!raw) return {}
    return coerceSessionImagePreviews(JSON.parse(raw))
  } catch {
    return {}
  }
}

export function saveSessionImagePreviews(sessionId: string, previews: SessionImagePreviews): void {
  const storageKey = getSessionImagePreviewKey(sessionId)
  if (!storageKey) return
  try {
    if (Object.keys(previews).length === 0) {
      sessionStorage.removeItem(storageKey)
      return
    }
    sessionStorage.setItem(storageKey, JSON.stringify(previews))
  } catch {
    /* ignore */
  }
}

/** Inline image previews are data URLs, so read the file once and cache it. */
export function readFileAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => {
      if (typeof reader.result === 'string') {
        resolve(reader.result)
        return
      }
      reject(new Error('image preview unavailable'))
    }
    reader.onerror = () => {
      reject(reader.error ?? new Error('image preview unavailable'))
    }
    reader.readAsDataURL(file)
  })
}
