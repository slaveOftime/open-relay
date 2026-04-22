import type { UpdateSessionMetadataSpec } from '@/api/types'

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

export function normalizeSessionTitleInput(input: string | null | undefined): string | null {
  const trimmed = input?.trim() ?? ''
  return trimmed === '' ? null : trimmed
}

function sameTags(left: string[], right: string[]): boolean {
  return left.length === right.length && left.every((tag, index) => tag === right[index])
}

export function buildSessionMetadataUpdateSpec(
  initial: { title: string | null; tags: string[] },
  draft: { title: string; tags: string }
): UpdateSessionMetadataSpec {
  const next: UpdateSessionMetadataSpec = {}

  const initialTitle = normalizeSessionTitleInput(initial.title)
  const nextTitle = normalizeSessionTitleInput(draft.title)
  if (initialTitle !== nextTitle) {
    next.title = nextTitle ?? ''
  }

  const initialTags = normalizeSessionTags(initial.tags)
  const nextTags = parseSessionTagInput(draft.tags)
  if (!sameTags(initialTags, nextTags)) {
    next.tags = nextTags
  }

  return next
}
