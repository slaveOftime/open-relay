import { useCallback, useEffect, useRef, useState } from 'react'
import { RotateCcw, ZoomIn, ZoomOut } from 'lucide-react'

import { Button } from '@/components/ui/button'
import { Dialog, DialogContent, DialogTitle } from '@/components/ui/dialog'
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'

import {
  clampImagePreviewOffset,
  clampImagePreviewZoom,
  getImagePreviewPanLimit,
  IMAGE_PREVIEW_ZOOM_STEP,
  type ImagePreviewDimensions,
  type ImagePreviewOffset,
} from './image-preview-dialog-state'

interface ImagePreviewDialogProps {
  open: boolean
  path: string | null
  src: string | null
  onOpenChange: (open: boolean) => void
}

export default function ImagePreviewDialog({
  open,
  path,
  src,
  onOpenChange,
}: ImagePreviewDialogProps) {
  const [zoom, setZoom] = useState(1)
  const [offset, setOffset] = useState<ImagePreviewOffset>({ x: 0, y: 0 })
  const [viewportSize, setViewportSize] = useState<ImagePreviewDimensions>({ width: 0, height: 0 })
  const [imageSize, setImageSize] = useState<ImagePreviewDimensions>({ width: 0, height: 0 })
  const viewportRef = useRef<HTMLDivElement | null>(null)
  const dragStateRef = useRef<{
    pointerId: number
    startX: number
    startY: number
    originX: number
    originY: number
  } | null>(null)

  const measureViewport = useCallback(() => {
    const viewport = viewportRef.current
    if (!viewport) return
    const { width, height } = viewport.getBoundingClientRect()
    setViewportSize({ width, height })
  }, [])

  const resetView = useCallback(() => {
    dragStateRef.current = null
    setZoom(1)
    setOffset({ x: 0, y: 0 })
  }, [])

  useEffect(() => {
    if (!open) return
    resetView()
    measureViewport()
    window.addEventListener('resize', measureViewport)
    return () => {
      window.removeEventListener('resize', measureViewport)
    }
  }, [measureViewport, open, resetView, src])

  useEffect(() => {
    if (!open) {
      dragStateRef.current = null
    }
  }, [open])

  const applyZoom = useCallback(
    (nextZoom: number) => {
      const clampedZoom = clampImagePreviewZoom(nextZoom)
      setZoom(clampedZoom)
      setOffset((previous) => {
        if (clampedZoom === 1) {
          return { x: 0, y: 0 }
        }
        return clampImagePreviewOffset(
          previous,
          getImagePreviewPanLimit(viewportSize, imageSize, clampedZoom)
        )
      })
    },
    [imageSize, viewportSize]
  )

  const handlePointerDown = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      if (zoom <= 1) return
      dragStateRef.current = {
        pointerId: event.pointerId,
        startX: event.clientX,
        startY: event.clientY,
        originX: offset.x,
        originY: offset.y,
      }
      event.currentTarget.setPointerCapture(event.pointerId)
    },
    [offset.x, offset.y, zoom]
  )

  const handlePointerMove = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      const dragState = dragStateRef.current
      if (!dragState || dragState.pointerId !== event.pointerId) return
      const limit = getImagePreviewPanLimit(viewportSize, imageSize, zoom)
      setOffset(
        clampImagePreviewOffset(
          {
            x: dragState.originX + event.clientX - dragState.startX,
            y: dragState.originY + event.clientY - dragState.startY,
          },
          limit
        )
      )
    },
    [imageSize, viewportSize, zoom]
  )

  const handlePointerEnd = useCallback((event: React.PointerEvent<HTMLDivElement>) => {
    if (dragStateRef.current?.pointerId !== event.pointerId) return
    dragStateRef.current = null
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId)
    }
  }, [])

  const handleWheel = useCallback(
    (event: React.WheelEvent<HTMLDivElement>) => {
      if (!src) return
      event.preventDefault()
      applyZoom(zoom + (event.deltaY < 0 ? IMAGE_PREVIEW_ZOOM_STEP : -IMAGE_PREVIEW_ZOOM_STEP))
    },
    [applyZoom, src, zoom]
  )

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="left-0 top-0 flex h-screen max-h-screen w-screen max-w-none translate-x-0 translate-y-0 flex-col gap-0 rounded-none border-0 bg-black/95 p-0 text-white">
        <div className="flex items-start justify-between gap-3 border-b border-white/10 px-3 py-2 pr-12">
          <div className="min-w-0">
            <DialogTitle className="text-sm font-semibold text-white">Image preview</DialogTitle>
            {path && <p className="truncate text-[11px] leading-4 text-white/70">{path}</p>}
          </div>
          <div className="flex shrink-0 items-center gap-1.5">
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  className="h-8 w-8 rounded-full text-white/80 hover:bg-white/10 hover:text-white"
                  onClick={() => applyZoom(zoom - IMAGE_PREVIEW_ZOOM_STEP)}
                >
                  <ZoomOut className="h-4 w-4" />
                  <span className="sr-only">Zoom out</span>
                </Button>
              </TooltipTrigger>
              <TooltipContent>Zoom out</TooltipContent>
            </Tooltip>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  className="h-8 w-8 rounded-full text-white/80 hover:bg-white/10 hover:text-white"
                  onClick={() => applyZoom(zoom + IMAGE_PREVIEW_ZOOM_STEP)}
                >
                  <ZoomIn className="h-4 w-4" />
                  <span className="sr-only">Zoom in</span>
                </Button>
              </TooltipTrigger>
              <TooltipContent>Zoom in</TooltipContent>
            </Tooltip>
            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  type="button"
                  variant="ghost"
                  size="icon"
                  className="h-8 text-white/80 hover:bg-white/10 hover:text-white"
                  onClick={resetView}
                >
                  <span>Reset</span>
                </Button>
              </TooltipTrigger>
              <TooltipContent>Reset view</TooltipContent>
            </Tooltip>
            <span className="min-w-12 rounded-md border border-white/10 bg-white/5 px-2 py-1 text-right text-[11px] text-white/70">
              {Math.round(zoom * 100)}%
            </span>
          </div>
        </div>
        <div
          ref={viewportRef}
          className="relative min-h-0 flex-1 overflow-hidden touch-none"
          onPointerDown={handlePointerDown}
          onPointerMove={handlePointerMove}
          onPointerUp={handlePointerEnd}
          onPointerCancel={handlePointerEnd}
          onWheel={handleWheel}
        >
          <div className="flex h-full w-full items-center justify-center overflow-hidden">
            {src && (
              <img
                src={src}
                alt={path ?? 'Image preview'}
                draggable={false}
                className="max-h-full max-w-full select-none object-contain will-change-transform"
                style={{
                  cursor: zoom > 1 ? (dragStateRef.current ? 'grabbing' : 'grab') : 'default',
                  transform: `translate(${offset.x}px, ${offset.y}px) scale(${zoom})`,
                  transformOrigin: 'center center',
                }}
                onLoad={(event) => {
                  setImageSize({
                    width: event.currentTarget.naturalWidth,
                    height: event.currentTarget.naturalHeight,
                  })
                  measureViewport()
                  resetView()
                }}
              />
            )}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  )
}
