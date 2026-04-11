import { describe, expect, it } from 'vitest'

import { getFirstTransferredFile, hasTransferredFiles } from './file-transfer'

describe('file transfer helpers', () => {
  it('detects when transfer types include files', () => {
    expect(hasTransferredFiles({ types: ['text/plain', 'Files'] })).toBe(true)
    expect(hasTransferredFiles({ types: ['text/plain'] })).toBe(false)
  })

  it('returns the first direct file when present', () => {
    const image = new File(['image'], 'screenshot.png', { type: 'image/png' })
    const text = new File(['text'], 'notes.txt', { type: 'text/plain' })

    expect(getFirstTransferredFile({ files: [image, text] })).toBe(image)
  })

  it('falls back to file items for pasted clipboard images', () => {
    const image = new File(['image'], 'clipboard.png', { type: 'image/png' })

    expect(
      getFirstTransferredFile({
        items: [
          { kind: 'string', getAsFile: () => null },
          { kind: 'file', getAsFile: () => image },
        ],
      })
    ).toBe(image)
  })

  it('ignores transfers without files', () => {
    expect(
      getFirstTransferredFile({
        items: [{ kind: 'string', getAsFile: () => null }],
      })
    ).toBeNull()
  })
})
