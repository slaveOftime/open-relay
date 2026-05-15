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
