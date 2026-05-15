export interface TextSelectionRange {
  start: number
  end: number
}

export interface TextInsertionResult {
  value: string
  selection: TextSelectionRange
}

function clampSelection(value: string, selection: Partial<TextSelectionRange> = {}): TextSelectionRange {
  const max = value.length
  const rawStart = selection.start ?? max
  const rawEnd = selection.end ?? rawStart
  const start = Math.max(0, Math.min(rawStart, max))
  const end = Math.max(start, Math.min(rawEnd, max))
  return { start, end }
}

export function insertTextAtSelection(
  value: string,
  insertedText: string,
  selection?: Partial<TextSelectionRange>
): TextInsertionResult {
  const { start, end } = clampSelection(value, selection)
  const nextValue = `${value.slice(0, start)}${insertedText}${value.slice(end)}`
  const caret = start + insertedText.length
  return {
    value: nextValue,
    selection: {
      start: caret,
      end: caret,
    },
  }
}

export function insertUploadedPathAtSelection(
  value: string,
  uploadedPath: string,
  selection?: Partial<TextSelectionRange>
): TextInsertionResult {
  const normalizedSelection = clampSelection(value, selection)
  const shouldPreserveAppendSpacing =
    value.trim().length > 0 &&
    normalizedSelection.start === normalizedSelection.end &&
    normalizedSelection.start === value.length

  return insertTextAtSelection(
    value,
    shouldPreserveAppendSpacing ? ` ${uploadedPath}` : uploadedPath,
    normalizedSelection
  )
}

function isWhitespace(value: string): boolean {
  return /\s/.test(value)
}

export function removeUploadedPathFromInput(
  value: string,
  uploadedPath: string,
  selection?: Partial<TextSelectionRange>
): TextInsertionResult {
  if (!uploadedPath) {
    const currentSelection = clampSelection(value, selection)
    return { value, selection: currentSelection }
  }

  let nextValue = value
  let nextSelection = clampSelection(value, selection)
  let matchIndex = nextValue.indexOf(uploadedPath)

  while (matchIndex !== -1) {
    let removeStart = matchIndex
    let removeEnd = matchIndex + uploadedPath.length
    const hasWhitespaceBefore = removeStart > 0 && isWhitespace(nextValue[removeStart - 1])
    const hasWhitespaceAfter = removeEnd < nextValue.length && isWhitespace(nextValue[removeEnd])

    if (hasWhitespaceBefore && (hasWhitespaceAfter || removeEnd === nextValue.length)) {
      removeStart -= 1
    } else if (hasWhitespaceAfter && removeStart === 0) {
      removeEnd += 1
    }

    const removedLength = removeEnd - removeStart
    nextValue = `${nextValue.slice(0, removeStart)}${nextValue.slice(removeEnd)}`
    nextSelection = {
      start: adjustSelectionAfterRemoval(nextSelection.start, removeStart, removeEnd, removedLength),
      end: adjustSelectionAfterRemoval(nextSelection.end, removeStart, removeEnd, removedLength),
    }
    matchIndex = nextValue.indexOf(uploadedPath)
  }

  return {
    value: nextValue,
    selection: nextSelection,
  }
}

function adjustSelectionAfterRemoval(
  position: number,
  removeStart: number,
  removeEnd: number,
  removedLength: number
): number {
  if (position <= removeStart) return position
  if (position >= removeEnd) return position - removedLength
  return removeStart
}
