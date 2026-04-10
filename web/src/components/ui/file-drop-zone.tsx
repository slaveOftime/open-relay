import * as React from 'react'
import { cn } from '@/lib/utils'

type FileDropZoneRenderState = {
  isDragOver: boolean
}

export interface FileDropZoneProps extends Omit<
  React.HTMLAttributes<HTMLDivElement>,
  'children' | 'onDrop' | 'onDragEnter' | 'onDragLeave' | 'onDragOver'
> {
  disabled?: boolean
  onFileDrop?: (file: File, files: FileList) => void | Promise<void>
  children: React.ReactNode | ((state: FileDropZoneRenderState) => React.ReactNode)
}

function hasDraggedFiles(event: React.DragEvent<HTMLDivElement>) {
  return Array.from(event.dataTransfer.types).includes('Files')
}

export const FileDropZone = React.forwardRef<HTMLDivElement, FileDropZoneProps>(
  ({ className, disabled = false, onFileDrop, children, ...props }, ref) => {
    const [isDragOver, setIsDragOver] = React.useState(false)
    const dragDepthRef = React.useRef(0)

    React.useEffect(() => {
      if (!disabled) return
      dragDepthRef.current = 0
      setIsDragOver(false)
    }, [disabled])

    function handleDragEnter(event: React.DragEvent<HTMLDivElement>) {
      if (disabled || !hasDraggedFiles(event)) return
      event.preventDefault()
      dragDepthRef.current += 1
      setIsDragOver(true)
    }

    function handleDragOver(event: React.DragEvent<HTMLDivElement>) {
      if (disabled || !hasDraggedFiles(event)) return
      event.preventDefault()
      event.dataTransfer.dropEffect = 'copy'
    }

    function handleDragLeave(event: React.DragEvent<HTMLDivElement>) {
      if (disabled || !hasDraggedFiles(event)) return
      event.preventDefault()
      dragDepthRef.current = Math.max(dragDepthRef.current - 1, 0)
      if (dragDepthRef.current === 0) {
        setIsDragOver(false)
      }
    }

    function handleDrop(event: React.DragEvent<HTMLDivElement>) {
      if (disabled || !hasDraggedFiles(event)) return
      event.preventDefault()
      dragDepthRef.current = 0
      setIsDragOver(false)

      const files = event.dataTransfer.files
      const file = files?.[0]
      if (!file || !onFileDrop) return
      void onFileDrop(file, files)
    }

    return (
      <div
        ref={ref}
        data-disabled={disabled ? 'true' : 'false'}
        data-drag-active={isDragOver ? 'true' : 'false'}
        className={cn(className)}
        onDragEnter={handleDragEnter}
        onDragOver={handleDragOver}
        onDragLeave={handleDragLeave}
        onDrop={handleDrop}
        {...props}
      >
        {typeof children === 'function' ? children({ isDragOver }) : children}
      </div>
    )
  }
)

FileDropZone.displayName = 'FileDropZone'
