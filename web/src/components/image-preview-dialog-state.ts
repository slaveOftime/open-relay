export interface ImagePreviewDimensions {
  width: number
  height: number
}

export interface ImagePreviewOffset {
  x: number
  y: number
}

export const MIN_IMAGE_PREVIEW_ZOOM = 1
export const MAX_IMAGE_PREVIEW_ZOOM = 6
export const IMAGE_PREVIEW_ZOOM_STEP = 0.25

export function clampImagePreviewZoom(value: number): number {
  return Math.min(MAX_IMAGE_PREVIEW_ZOOM, Math.max(MIN_IMAGE_PREVIEW_ZOOM, value))
}

export function getImagePreviewPanLimit(
  viewport: ImagePreviewDimensions,
  image: ImagePreviewDimensions,
  zoom: number
): ImagePreviewOffset {
  if (
    viewport.width <= 0 ||
    viewport.height <= 0 ||
    image.width <= 0 ||
    image.height <= 0
  ) {
    return { x: 0, y: 0 }
  }

  const fitScale = Math.min(1, viewport.width / image.width, viewport.height / image.height)
  const scaledWidth = image.width * fitScale * clampImagePreviewZoom(zoom)
  const scaledHeight = image.height * fitScale * clampImagePreviewZoom(zoom)

  return {
    x: Math.max(0, (scaledWidth - viewport.width) / 2),
    y: Math.max(0, (scaledHeight - viewport.height) / 2),
  }
}

export function clampImagePreviewOffset(
  offset: ImagePreviewOffset,
  limit: ImagePreviewOffset
): ImagePreviewOffset {
  return {
    x: Math.min(limit.x, Math.max(-limit.x, offset.x)),
    y: Math.min(limit.y, Math.max(-limit.y, offset.y)),
  }
}
