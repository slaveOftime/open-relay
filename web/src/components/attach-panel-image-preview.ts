export interface SessionImagePreviews {
  [path: string]: string
}

export function coerceSessionImagePreviews(raw: unknown): SessionImagePreviews {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) {
    return {}
  }

  const previews: SessionImagePreviews = {}
  for (const [path, source] of Object.entries(raw)) {
    if (path.length === 0 || typeof source !== 'string' || source.length === 0) {
      continue
    }
    previews[path] = source
  }

  return previews
}

export function getVisibleImagePreviewPaths(
  value: string,
  previews: SessionImagePreviews
): string[] {
  return Object.keys(previews)
    .filter((path) => value.includes(path))
    .sort((left, right) => value.indexOf(left) - value.indexOf(right) || left.localeCompare(right))
}

export function isPreviewableImageFile(file: Pick<File, 'type'>): boolean {
  return file.type.trim().toLowerCase().startsWith('image/')
}
