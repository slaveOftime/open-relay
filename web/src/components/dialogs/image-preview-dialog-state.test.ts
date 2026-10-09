import { describe, expect, it } from 'vitest'

import {
  clampImagePreviewOffset,
  clampImagePreviewZoom,
  getImagePreviewPanLimit,
} from './image-preview-dialog-state'

describe('image preview dialog state helpers', () => {
  it('clamps zoom to the supported range', () => {
    expect(clampImagePreviewZoom(0.5)).toBe(1)
    expect(clampImagePreviewZoom(2.5)).toBe(2.5)
    expect(clampImagePreviewZoom(10)).toBe(6)
  })

  it('computes pan limits from the fitted image size', () => {
    expect(
      getImagePreviewPanLimit({ width: 800, height: 600 }, { width: 1600, height: 1200 }, 2)
    ).toEqual({
      x: 400,
      y: 300,
    })
  })

  it('clamps pan offsets to the current limits', () => {
    expect(clampImagePreviewOffset({ x: 200, y: -150 }, { x: 80, y: 40 })).toEqual({
      x: 80,
      y: -40,
    })
  })
})
