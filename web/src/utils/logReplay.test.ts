import { describe, expect, it } from 'vitest'
import xtermPkg from '@xterm/xterm'
import codexLog from '../../../tests/output-codex.log?raw'
import opencodeLog from '../../../tests/output-opencode.log?raw'
import {
  appendLogChunks,
  encodeLogChunks,
  initialLogReplayState,
  replayLogChunks,
} from './logReplay'

const { Terminal } = xtermPkg as typeof import('@xterm/xterm')
const decoder = new TextDecoder()

function readFixtureChunks(text: string): Uint8Array[] {
  return encodeLogChunks(text.match(/[^\n]*\n|[^\n]+$/g) ?? [])
}

async function flushTerminal(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 10))
  await new Promise((resolve) => setTimeout(resolve, 10))
}

function visibleBuffer(term: InstanceType<typeof Terminal>): string {
  return Array.from(
    { length: term.rows },
    (_, index) => term.buffer.active.getLine(index)?.translateToString(true) ?? ''
  ).join('\n')
}

describe('logReplay', () => {
  it('applies resize events at cumulative byte offsets', () => {
    const operations: string[] = []
    const target = {
      reset: () => operations.push('reset'),
      write: (data: string | Uint8Array) =>
        operations.push(`write:${typeof data === 'string' ? data : decoder.decode(data)}`),
      resize: (cols: number, rows: number) => operations.push(`resize:${cols}x${rows}`),
      scrollToBottom: () => operations.push('scroll'),
    }

    const state = replayLogChunks(target, encodeLogChunks(['ab', 'cdef', 'g']), [
      { offset: 0, rows: 24, cols: 80 },
      { offset: 2, rows: 30, cols: 90 },
      { offset: 6, rows: 40, cols: 100 },
    ])

    expect(operations).toEqual([
      'reset',
      'resize:80x24',
      'write:ab',
      'resize:90x30',
      'write:cdef',
      'resize:100x40',
      'write:g',
      'scroll',
    ])
    expect(state).toEqual({ bytesWritten: 7, nextResizeIndex: 3, chunkCount: 3 })
  })

  it('batches adjacent chunks into fewer terminal writes', () => {
    const writes: string[] = []
    const target = {
      write: (data: string | Uint8Array) =>
        writes.push(typeof data === 'string' ? data : decoder.decode(data)),
      resize: () => {},
    }

    const state = appendLogChunks(
      target,
      encodeLogChunks(['ab', 'cd', 'ef']),
      [],
      initialLogReplayState()
    )

    expect(writes).toEqual(['abcdef'])
    expect(state).toEqual({ bytesWritten: 6, nextResizeIndex: 0, chunkCount: 3 })
  })

  it('can append additional log pages while preserving resize progress', () => {
    const operations: string[] = []
    const target = {
      write: (data: string | Uint8Array) =>
        operations.push(`write:${typeof data === 'string' ? data : decoder.decode(data)}`),
      resize: (cols: number, rows: number) => operations.push(`resize:${cols}x${rows}`),
    }

    let state = initialLogReplayState()
    state = appendLogChunks(
      target,
      encodeLogChunks(['ab']),
      [
        { offset: 0, rows: 24, cols: 80 },
        { offset: 4, rows: 30, cols: 90 },
      ],
      state
    )
    state = appendLogChunks(
      target,
      encodeLogChunks(['cd', 'ef']),
      [
        { offset: 0, rows: 24, cols: 80 },
        { offset: 4, rows: 30, cols: 90 },
      ],
      state
    )

    expect(operations).toEqual(['resize:80x24', 'write:ab', 'write:cd', 'resize:90x30', 'write:ef'])
    expect(state).toEqual({ bytesWritten: 6, nextResizeIndex: 2, chunkCount: 3 })
  })

  it('replays the codex log fixture into xterm', async () => {
    const term = new Terminal({ cols: 105, rows: 37, scrollback: 2000 })
    replayLogChunks(
      {
        reset: () => term.reset(),
        write: (data: string | Uint8Array) => term.write(data),
        resize: (cols: number, rows: number) => term.resize(cols, rows),
        scrollToBottom: () => term.scrollToBottom(),
      },
      readFixtureChunks(codexLog),
      [{ offset: 0, rows: 37, cols: 105 }]
    )
    await flushTerminal()

    const visible = visibleBuffer(term)
    expect(visible).toContain('Select Model')
    expect(visible).toContain('Claude Sonnet 4.5')
    expect(visible).toContain('GPT-5.4')
    expect(visible).toContain('Enter to select')
  })

  it('replays the opencode log fixture into xterm', async () => {
    const term = new Terminal({ cols: 105, rows: 37, scrollback: 2000 })
    replayLogChunks(
      {
        reset: () => term.reset(),
        write: (data: string | Uint8Array) => term.write(data),
        resize: (cols: number, rows: number) => term.resize(cols, rows),
        scrollToBottom: () => term.scrollToBottom(),
      },
      readFixtureChunks(opencodeLog),
      [{ offset: 0, rows: 37, cols: 105 }]
    )
    await flushTerminal()

    const visible = visibleBuffer(term)
    expect(visible).toContain('tell me')
    expect(visible).toContain('OpenCode Zen')
    expect(visible).toContain('doom_loop')
  })
})
