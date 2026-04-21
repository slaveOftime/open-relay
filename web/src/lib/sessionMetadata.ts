export function normalizeSessionTags(tags: string[]): string[] {
  const seen = new Set<string>()
  const normalized: string[] = []
  for (const tag of tags) {
    const trimmed = tag.trim()
    if (trimmed === '') continue
    const key = trimmed.toLowerCase()
    if (seen.has(key)) continue
    seen.add(key)
    normalized.push(trimmed)
  }
  return normalized
}

export function parseSessionTagInput(input: string): string[] {
  return normalizeSessionTags(input.split(/[,\n]/))
}

export function formatSessionTagInput(tags: string[]): string {
  return normalizeSessionTags(tags).join(', ')
}
