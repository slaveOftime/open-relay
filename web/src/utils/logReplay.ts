import type { LogResizeEvent } from '@/api/types'

export interface LogReplayTarget {
  reset(): void
  write(data: string): void
  resize(cols: number, rows: number): void
  scrollToBottom(): void
}

export interface LogReplayState {
  bytesWritten: number
  nextResizeIndex: number
  chunkCount: number
}

const encoder = new TextEncoder()

export function initialLogReplayState(): LogReplayState {
  return {
    bytesWritten: 0,
    nextResizeIndex: 0,
    chunkCount: 0,
  }
}

function applyPendingResizes(
  target: Pick<LogReplayTarget, 'resize'>,
  resizes: LogResizeEvent[],
  state: LogReplayState
): void {
  while (state.nextResizeIndex < resizes.length) {
    const resize = resizes[state.nextResizeIndex]
    if (resize.offset > state.bytesWritten) break
    target.resize(resize.cols, resize.rows)
    state.nextResizeIndex += 1
  }
}

export function appendLogChunks(
  target: Pick<LogReplayTarget, 'write' | 'resize'>,
  chunks: string[],
  resizes: LogResizeEvent[],
  state: LogReplayState
): LogReplayState {
  return appendLogChunkRange(target, chunks, resizes, state, 0, chunks.length)
}

function appendLogChunkRange(
  target: Pick<LogReplayTarget, 'write' | 'resize'>,
  chunks: string[],
  resizes: LogResizeEvent[],
  state: LogReplayState,
  startChunk: number,
  endChunk: number
): LogReplayState {
  const safeStart = Math.max(0, Math.min(startChunk, chunks.length))
  const safeEnd = Math.max(safeStart, Math.min(endChunk, chunks.length))
  const next: LogReplayState = { ...state }

  applyPendingResizes(target, resizes, next)
  for (let index = safeStart; index < safeEnd; index += 1) {
    const chunk = chunks[index]
    applyPendingResizes(target, resizes, next)
    target.write(chunk)
    next.bytesWritten += encoder.encode(chunk).length
    next.chunkCount += 1
  }
  applyPendingResizes(target, resizes, next)

  return next
}

export function replayLogChunks(
  target: LogReplayTarget,
  chunks: string[],
  resizes: LogResizeEvent[],
  chunkCount = chunks.length
): LogReplayState {
  const safeChunkCount = Math.max(0, Math.min(chunkCount, chunks.length))
  target.reset()
  const state = appendLogChunkRange(
    target,
    chunks,
    resizes,
    initialLogReplayState(),
    0,
    safeChunkCount
  )
  target.scrollToBottom()
  return state
}

export function seekLogChunks(
  target: LogReplayTarget,
  chunks: string[],
  resizes: LogResizeEvent[],
  state: LogReplayState,
  chunkCount = chunks.length
): LogReplayState {
  const safeChunkCount = Math.max(0, Math.min(chunkCount, chunks.length))
  if (safeChunkCount <= state.chunkCount) {
    return replayLogChunks(target, chunks, resizes, safeChunkCount)
  }

  const next = appendLogChunkRange(target, chunks, resizes, state, state.chunkCount, safeChunkCount)
  target.scrollToBottom()
  return next
}
