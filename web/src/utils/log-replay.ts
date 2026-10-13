import type { LogResizeEvent } from '@/api/types'

export type LogReplayChunk = Uint8Array

export interface LogReplayTarget {
  reset(): void
  write(data: string | Uint8Array, callback?: () => void): void
  resize(cols: number, rows: number): void
  scrollToBottom(): void
}

export interface LogReplayState {
  bytesWritten: number
  nextResizeIndex: number
  chunkCount: number
  chunkOffset: number
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
    chunkOffset: 0,
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
  pendingWrite: { chunks: LogReplayChunk[]; byteLength: number },
  callback?: () => void
): void {
  if (pendingWrite.byteLength === 0) {
    if (callback) callback()
    return
  }
  if (pendingWrite.chunks.length === 1) {
    target.write(pendingWrite.chunks[0], callback)
  } else {
    const merged = new Uint8Array(pendingWrite.byteLength)
    let offset = 0
    for (const chunk of pendingWrite.chunks) {
      merged.set(chunk, offset)
      offset += chunk.length
    }
    target.write(merged, callback)
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
  endChunk: number,
  options?: {
    callback?: () => void
    maxBytes?: number
  }
): LogReplayState {
  const safeStart = Math.max(0, Math.min(startChunk, chunks.length))
  const safeEnd = Math.max(safeStart, Math.min(endChunk, chunks.length))
  const next: LogReplayState = { ...state }
  const pendingWrite = { chunks: [] as LogReplayChunk[], byteLength: 0 }

  // If we are starting from a different chunk than the state indicates, reset offset
  if (next.chunkCount !== safeStart) {
    next.chunkCount = safeStart
    next.chunkOffset = 0
  }

  applyPendingResizesWithFlush(target, resizes, next, pendingWrite)

  let bytesAdded = 0
  const maxBytes = options?.maxBytes ?? Infinity

  for (let index = safeStart; index < safeEnd; index += 1) {
    if (bytesAdded >= maxBytes) break

    const fullChunk = chunks[index]
    const remainingInChunk = fullChunk.length - next.chunkOffset

    // Determine how much of this chunk we can add
    const bytesToTake = Math.min(remainingInChunk, maxBytes - bytesAdded)

    // Optimisation: If taking the whole remaining chunk, no copy/slice needed if offset is 0
    let chunkToAdd: LogReplayChunk
    if (next.chunkOffset === 0 && bytesToTake === fullChunk.length) {
      chunkToAdd = fullChunk
    } else {
      chunkToAdd = fullChunk.subarray(next.chunkOffset, next.chunkOffset + bytesToTake)
    }

    pendingWrite.chunks.push(chunkToAdd)
    pendingWrite.byteLength += chunkToAdd.length
    next.bytesWritten += chunkToAdd.length
    bytesAdded += chunkToAdd.length

    next.chunkOffset += bytesToTake

    // If we finished this chunk, move to next
    if (next.chunkOffset >= fullChunk.length) {
      next.chunkCount += 1
      next.chunkOffset = 0
    }

    if (pendingWrite.byteLength >= MAX_PENDING_WRITE_BYTES) {
      // Intermediate flushes don't fire the final callback
      flushPendingWrite(target, pendingWrite)
    }
    applyPendingResizesWithFlush(target, resizes, next, pendingWrite)
  }
  // The final flush fires the callback
  flushPendingWrite(target, pendingWrite, options?.callback)
  applyPendingResizes(target, resizes, next)

  return next
}

export function replayLogChunks(
  target: LogReplayTarget,
  chunks: LogReplayChunk[],
  resizes: LogResizeEvent[],
  chunkCount = chunks.length,
  options?: { scrollToBottom?: boolean }
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
  if (options?.scrollToBottom !== false) {
    target.scrollToBottom()
  }
  return state
}

export function seekLogChunks(
  target: LogReplayTarget,
  chunks: LogReplayChunk[],
  resizes: LogResizeEvent[],
  state: LogReplayState,
  chunkCount = chunks.length,
  options?: { scrollToBottom?: boolean; callback?: () => void }
): LogReplayState {
  const safeChunkCount = Math.max(0, Math.min(chunkCount, chunks.length))
  if (safeChunkCount <= state.chunkCount) {
    const next = replayLogChunks(target, chunks, resizes, safeChunkCount, {
      scrollToBottom: options?.scrollToBottom,
    })
    if (options?.callback) options.callback()
    return next
  }

  const next = appendLogChunkRange(
    target,
    chunks,
    resizes,
    state,
    state.chunkCount,
    safeChunkCount,
    { callback: options?.callback }
  )
  if (options?.scrollToBottom !== false) {
    target.scrollToBottom()
  }
  return next
}

export function playNextBatch(
  target: LogReplayTarget,
  chunks: LogReplayChunk[],
  resizes: LogResizeEvent[],
  state: LogReplayState,
  maxBytes: number,
  callback?: () => void
): LogReplayState {
  return appendLogChunkRange(
    target,
    chunks,
    resizes,
    state,
    state.chunkCount, // Start from current chunk
    chunks.length, // Try to go to end, but maxBytes will stop us
    { callback, maxBytes }
  )
}
