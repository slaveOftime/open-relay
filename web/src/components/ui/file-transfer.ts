type FileTransferItemLike = {
  kind?: string
  getAsFile?: () => File | null
}

type FileTransferDataLike = {
  files?: ArrayLike<File> | null
  items?: ArrayLike<FileTransferItemLike> | null
  types?: ArrayLike<string> | Iterable<string> | null
}

export function hasTransferredFiles(data: Pick<FileTransferDataLike, 'types'> | null | undefined) {
  const types = data?.types
  if (!types) return false
  return Array.from(types).includes('Files')
}

export function getTransferredFiles(
  data: Pick<FileTransferDataLike, 'files' | 'items'> | null | undefined
): File[] {
  const directFiles = Array.from(data?.files ?? []).filter((file): file is File => Boolean(file))
  if (directFiles.length > 0) return directFiles

  const files: File[] = []
  for (const item of Array.from(data?.items ?? [])) {
    if (item.kind !== 'file') continue
    const file = item.getAsFile?.()
    if (file) files.push(file)
  }

  return files
}

export function getFirstTransferredFile(
  data: Pick<FileTransferDataLike, 'files' | 'items'> | null | undefined
): File | null {
  return getTransferredFiles(data)[0] ?? null
}
