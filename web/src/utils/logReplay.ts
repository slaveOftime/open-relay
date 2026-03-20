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
}

const encoder = new TextEncoder()

export function initialLogReplayState(): LogReplayState {
  return {
    bytesWritten: 0,
    nextResizeIndex: 0,
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
    console.info(`Applying log resize: ${resize.cols} cols, ${resize.rows} rows (offset: ${resize.offset})`)
    target.resize(resize.cols, resize.rows)
    state.nextResizeIndex += 1
  }
}

export function appendLogLines(
  target: Pick<LogReplayTarget, 'write' | 'resize'>,
  lines: string[],
  resizes: LogResizeEvent[],
  state: LogReplayState
): LogReplayState {
  const next: LogReplayState = { ...state }

  applyPendingResizes(target, resizes, next)
  for (const line of lines) {
    applyPendingResizes(target, resizes, next)
    target.write(line)
    next.bytesWritten += encoder.encode(line).length
  }
  applyPendingResizes(target, resizes, next)

  return next
}

export function replayLogLines(
  target: LogReplayTarget,
  lines: string[],
  resizes: LogResizeEvent[],
  lineCount = lines.length
): LogReplayState {
  target.reset()
  const state = appendLogLines(target, lines.slice(0, lineCount), resizes, initialLogReplayState())
  target.scrollToBottom()
  return state
}
