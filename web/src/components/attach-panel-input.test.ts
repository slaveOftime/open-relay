import { describe, expect, it } from 'vitest'

import {
  insertTextAtSelection,
  insertUploadedPathAtSelection,
  removeUploadedPathFromInput,
} from './attach-panel-input'

describe('attach-panel input helpers', () => {
  it('inserts uploaded paths at the current cursor position', () => {
    expect(insertUploadedPathAtSelection('echo hello', '/tmp/paste.png', { start: 5, end: 5 })).toEqual({
      value: 'echo /tmp/paste.pnghello',
      selection: {
        start: 19,
        end: 19,
      },
    })
  })

  it('replaces the current selection with the uploaded path', () => {
    expect(insertUploadedPathAtSelection('echo hello', '/tmp/paste.png', { start: 5, end: 10 })).toEqual({
      value: 'echo /tmp/paste.png',
      selection: {
        start: 19,
        end: 19,
      },
    })
  })

  it('preserves the existing append spacing when the cursor is already at the end', () => {
    expect(
      insertUploadedPathAtSelection('echo hello', '/tmp/paste.png', { start: 10, end: 10 })
    ).toEqual({
      value: 'echo hello /tmp/paste.png',
      selection: {
        start: 25,
        end: 25,
      },
    })
  })

  it('clamps insertion ranges before replacing text', () => {
    expect(insertTextAtSelection('hello', 'X', { start: -2, end: 99 })).toEqual({
      value: 'X',
      selection: {
        start: 1,
        end: 1,
      },
    })
  })

  it('removes uploaded paths and their append spacing from input text', () => {
    expect(
      removeUploadedPathFromInput('echo hello /tmp/paste.png /tmp/other.png', '/tmp/paste.png', {
        start: 36,
        end: 36,
      })
    ).toEqual({
      value: 'echo hello /tmp/other.png',
      selection: {
        start: 21,
        end: 21,
      },
    })
  })

  it('removes every occurrence so hidden previews stay removed', () => {
    expect(removeUploadedPathFromInput('/tmp/paste.png cat /tmp/paste.png', '/tmp/paste.png')).toEqual({
      value: 'cat',
      selection: {
        start: 3,
        end: 3,
      },
    })
  })
})
