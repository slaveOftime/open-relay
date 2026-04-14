import { describe, expect, it } from 'vitest'

import { getFirstTransferredFile, getTransferredFiles, hasTransferredFiles } from './file-transfer'

describe('file transfer helpers', () => {
  it('detects when transfer types include files', () => {
    expect(hasTransferredFiles({ types: ['text/plain', 'Files'] })).toBe(true)
    expect(hasTransferredFiles({ types: ['text/plain'] })).toBe(false)
  })

  it('returns direct files in order when present', () => {
    const image = new File(['image'], 'screenshot.png', { type: 'image/png' })
    const text = new File(['text'], 'notes.txt', { type: 'text/plain' })

    expect(getTransferredFiles({ files: [image, text] })).toEqual([image, text])
    expect(getFirstTransferredFile({ files: [image, text] })).toBe(image)
  })

  it('falls back to all file items for pasted clipboard files', () => {
    const image = new File(['image'], 'clipboard.png', { type: 'image/png' })
    const text = new File(['text'], 'clipboard.txt', { type: 'text/plain' })

    expect(
      getTransferredFiles({
        items: [
          { kind: 'string', getAsFile: () => null },
          { kind: 'file', getAsFile: () => image },
          { kind: 'file', getAsFile: () => text },
        ],
      })
    ).toEqual([image, text])

    expect(
      getFirstTransferredFile({
        items: [
          { kind: 'string', getAsFile: () => null },
          { kind: 'file', getAsFile: () => image },
          { kind: 'file', getAsFile: () => text },
        ],
      })
    ).toBe(image)
  })

  it('ignores transfers without files', () => {
    expect(
      getTransferredFiles({
        items: [{ kind: 'string', getAsFile: () => null }],
      })
    ).toEqual([])

    expect(
      getFirstTransferredFile({
        items: [{ kind: 'string', getAsFile: () => null }],
      })
    ).toBeNull()
  })
})
