import { describe, expect, it } from 'vitest'

import {
  coerceSessionImagePreviews,
  getVisibleImagePreviewPaths,
  isPreviewableImageFile,
} from './attach-panel-image-preview'

describe('attach-panel image preview helpers', () => {
  it('keeps only string preview sources when loading stored previews', () => {
    expect(
      coerceSessionImagePreviews({
        '/tmp/paste.png': 'data:image/png;base64,abc',
        '/tmp/empty.png': '',
        '/tmp/bad.png': 123,
      })
    ).toEqual({
      '/tmp/paste.png': 'data:image/png;base64,abc',
    })
  })

  it('returns visible preview paths in input order', () => {
    expect(
      getVisibleImagePreviewPaths('cat /tmp/two.png && cat /tmp/one.png', {
        '/tmp/one.png': 'data:image/png;base64,one',
        '/tmp/two.png': 'data:image/png;base64,two',
        '/tmp/missing.png': 'data:image/png;base64,missing',
      })
    ).toEqual(['/tmp/two.png', '/tmp/one.png'])
  })

  it('recognizes previewable image files by mime type', () => {
    expect(isPreviewableImageFile({ type: ' image/png ' })).toBe(true)
    expect(isPreviewableImageFile({ type: 'text/plain' })).toBe(false)
  })
})
