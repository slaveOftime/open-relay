/**
 * Frequency-ranked history of what the user typed into the attach panel.
 *
 * Every send records the trimmed text and bumps its count, and the drawer
 * lists the most-used entries first. Storage failures are ignored: history
 * is a convenience, so an unavailable cache just starts empty.
 */

const INPUT_HISTORY_KEY = 'open-relay:input-history'

export interface InputHistoryEntry {
  text: string
  count: number
}

export function loadInputHistory(): InputHistoryEntry[] {
  try {
    const raw = localStorage.getItem(INPUT_HISTORY_KEY)
    if (!raw) return []
    return JSON.parse(raw) as InputHistoryEntry[]
  } catch {
    return []
  }
}

export function saveInputHistory(text: string): void {
  const trimmed = text.trim()
  if (!trimmed) return
  try {
    const history = loadInputHistory()
    const existing = history.find((e) => e.text === trimmed)
    if (existing) existing.count += 1
    else history.push({ text: trimmed, count: 1 })
    history.sort((a, b) => b.count - a.count)
    localStorage.setItem(INPUT_HISTORY_KEY, JSON.stringify(history.slice(0, 50)))
  } catch {
    /* ignore */
  }
}
