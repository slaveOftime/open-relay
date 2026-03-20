import type { LogResizeEvent } from '@/api/types'

export type LogReplayChunk = Uint8Array

export interface LogReplayTarget {
  reset(): void
  write(data: string | Uint8Array): void
  resize(cols: number, rows: number): void
  scrollToBottom(): void
}

export interface LogReplayState {
  bytesWritten: number
  nextResizeIndex: number
  chunkCount: number
}

const encoder = new TextEncoder()
const MAX_PENDING_WRITE_BYTES = 32 * 1024

export function encodeLogChunks(chunks: string[]): LogReplayChunk[] {
  return chunks.map((chunk) => encoder.encode(chunk))
}

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

function flushPendingWrite(
  target: Pick<LogReplayTarget, 'write'>,
  pendingWrite: { chunks: LogReplayChunk[]; byteLength: number }
): void {
  if (pendingWrite.byteLength === 0) return
  if (pendingWrite.chunks.length === 1) {
    target.write(pendingWrite.chunks[0])
  } else {
    const merged = new Uint8Array(pendingWrite.byteLength)
    let offset = 0
    for (const chunk of pendingWrite.chunks) {
      merged.set(chunk, offset)
      offset += chunk.length
    }
    target.write(merged)
  }
  pendingWrite.chunks = []
  pendingWrite.byteLength = 0
}

function applyPendingResizesWithFlush(
  target: Pick<LogReplayTarget, 'write' | 'resize'>,
  resizes: LogResizeEvent[],
  state: LogReplayState,
  pendingWrite: { chunks: LogReplayChunk[]; byteLength: number }
): void {
  while (state.nextResizeIndex < resizes.length) {
    const resize = resizes[state.nextResizeIndex]
    if (resize.offset > state.bytesWritten) break
    flushPendingWrite(target, pendingWrite)
    target.resize(resize.cols, resize.rows)
    state.nextResizeIndex += 1
  }
}

export function appendLogChunks(
  target: Pick<LogReplayTarget, 'write' | 'resize'>,
  chunks: LogReplayChunk[],
  resizes: LogResizeEvent[],
  state: LogReplayState
): LogReplayState {
  return appendLogChunkRange(target, chunks, resizes, state, 0, chunks.length)
}

function appendLogChunkRange(
  target: Pick<LogReplayTarget, 'write' | 'resize'>,
  chunks: LogReplayChunk[],
  resizes: LogResizeEvent[],
  state: LogReplayState,
  startChunk: number,
  endChunk: number
): LogReplayState {
  const safeStart = Math.max(0, Math.min(startChunk, chunks.length))
  const safeEnd = Math.max(safeStart, Math.min(endChunk, chunks.length))
  const next: LogReplayState = { ...state }
  const pendingWrite = { chunks: [] as LogReplayChunk[], byteLength: 0 }

  applyPendingResizesWithFlush(target, resizes, next, pendingWrite)
  for (let index = safeStart; index < safeEnd; index += 1) {
    const chunk = chunks[index]
    pendingWrite.chunks.push(chunk)
    pendingWrite.byteLength += chunk.length
    next.bytesWritten += chunk.length
    next.chunkCount += 1
    if (pendingWrite.byteLength >= MAX_PENDING_WRITE_BYTES) {
      flushPendingWrite(target, pendingWrite)
    }
    applyPendingResizesWithFlush(target, resizes, next, pendingWrite)
  }
  flushPendingWrite(target, pendingWrite)
  applyPendingResizes(target, resizes, next)

  return next
}

export function replayLogChunks(
  target: LogReplayTarget,
  chunks: LogReplayChunk[],
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
  chunks: LogReplayChunk[],
  resizes: LogResizeEvent[],
  state: LogReplayState,
  chunkCount = chunks.length,
  options?: { scrollToBottom?: boolean }
): LogReplayState {
  const safeChunkCount = Math.max(0, Math.min(chunkCount, chunks.length))
  if (safeChunkCount <= state.chunkCount) {
    return replayLogChunks(target, chunks, resizes, safeChunkCount)
  }

  const next = appendLogChunkRange(target, chunks, resizes, state, state.chunkCount, safeChunkCount)
  if (options?.scrollToBottom !== false) {
    target.scrollToBottom()
  }
  return next
}
