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

export function getFirstTransferredFile(
  data: Pick<FileTransferDataLike, 'files' | 'items'> | null | undefined
): File | null {
  const directFile = data?.files?.[0]
  if (directFile) return directFile

  for (const item of Array.from(data?.items ?? [])) {
    if (item.kind !== 'file') continue
    const file = item.getAsFile?.()
    if (file) return file
  }

  return null
}
