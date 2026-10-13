import { describe, expect, it, vi } from 'vitest'

import {
  sendCustomKeys,
  sendCustomText,
  sendKeySpec,
  type AttachSendTarget,
} from './attach-panel-send'

function target() {
  const calls = {
    sendInput: vi.fn(),
    showKeyError: vi.fn(),
    recordHistory: vi.fn(),
  }
  const t: AttachSendTarget = { ...calls }
  return { t, calls }
}

/**
 * The wire values and error strings here are the ones `key-input` produces, so
 * these tests double as documentation of what the user sees when a send is
 * rejected.
 */
describe('sendKeySpec', () => {
  it('sends the parsed sequence for a known spec', () => {
    const { t, calls } = target()

    // ctrl+c is ASCII 3 on the wire.
    sendKeySpec('ctrl+c', t)

    expect(calls.sendInput).toHaveBeenCalledOnce()
    expect(calls.sendInput).toHaveBeenCalledWith('\x03')
    expect(calls.showKeyError).not.toHaveBeenCalled()
  })

  it('reports the parser message for an unsupported spec', () => {
    const { t, calls } = target()

    sendKeySpec('not-a-key', t)

    expect(calls.sendInput).not.toHaveBeenCalled()
    expect(calls.showKeyError).toHaveBeenCalledOnce()
    expect(calls.showKeyError.mock.calls[0][0]).toContain('unsupported --key')
  })

  it('reports the message for a modifier sent on its own', () => {
    // The radial menu queues modifier buttons; tapping one and hitting send
    // without a key is exactly this case.
    const { t, calls } = target()

    sendKeySpec('ctrl', t)

    expect(calls.sendInput).not.toHaveBeenCalled()
    expect(calls.showKeyError.mock.calls[0][0]).toContain('modifier-only')
  })

  it('reports the message for an empty spec', () => {
    const { t, calls } = target()

    sendKeySpec('   ', t)

    expect(calls.sendInput).not.toHaveBeenCalled()
    expect(calls.showKeyError.mock.calls[0][0]).toContain('empty')
  })

  it('never throws: a bad spec is reported, not raised', () => {
    const { t } = target()

    expect(() => sendKeySpec('', t)).not.toThrow()
    expect(() => sendKeySpec('not-a-key', t)).not.toThrow()
    expect(() => sendKeySpec('ctrl', t)).not.toThrow()
  })
})

describe('sendCustomKeys', () => {
  it('sends one frame per spec, in order', () => {
    const { t, calls } = target()

    sendCustomKeys({ text: 'ctrl+c ctrl+d', onSent: () => {}, ...t })

    expect(calls.sendInput).toHaveBeenCalledTimes(2)
    expect(calls.sendInput).toHaveBeenNthCalledWith(1, '\x03')
    expect(calls.sendInput).toHaveBeenNthCalledWith(2, '\x04')
    expect(calls.showKeyError).not.toHaveBeenCalled()
  })

  it('queues a modifier against the key that follows it', () => {
    // "ctrl c" is one combined spec, the same as typing ctrl+c.
    const { t, calls } = target()

    sendCustomKeys({ text: 'ctrl c', onSent: () => {}, ...t })

    expect(calls.sendInput).toHaveBeenCalledOnce()
    expect(calls.sendInput).toHaveBeenCalledWith('\x03')
  })

  it('clears the composer only after every spec was accepted', () => {
    const onSent = vi.fn()
    const { t, calls } = target()

    sendCustomKeys({ text: 'ctrl+c', onSent, ...t })

    expect(onSent).toHaveBeenCalledOnce()
    expect(calls.showKeyError).not.toHaveBeenCalled()
  })

  it('does not clear the composer or send anything when a spec is rejected', () => {
    const onSent = vi.fn()
    const { t, calls } = target()

    sendCustomKeys({ text: 'ctrl+c not-a-key', onSent, ...t })

    expect(calls.sendInput).not.toHaveBeenCalled()
    expect(onSent).not.toHaveBeenCalled()
    expect(calls.showKeyError).toHaveBeenCalledOnce()
  })

  it('rejects a composer that ends on a modifier with no key after it', () => {
    const onSent = vi.fn()
    const { t, calls } = target()

    sendCustomKeys({ text: 'ctrl+c ctrl', onSent, ...t })

    expect(calls.sendInput).not.toHaveBeenCalled()
    expect(onSent).not.toHaveBeenCalled()
    expect(calls.showKeyError.mock.calls[0][0]).toContain('must be followed by a key value')
  })

  it('does nothing for an empty or whitespace-only composer', () => {
    const onSent = vi.fn()
    const { t, calls } = target()

    sendCustomKeys({ text: '', onSent, ...t })
    sendCustomKeys({ text: '   ', onSent, ...t })

    expect(calls.sendInput).not.toHaveBeenCalled()
    expect(onSent).not.toHaveBeenCalled()
  })
})

describe('sendCustomText', () => {
  it('sends the text as typed and records it in history', () => {
    const { t, calls } = target()

    sendCustomText({ text: 'ls -la', onSent: () => {}, ...t })

    expect(calls.sendInput).toHaveBeenCalledOnce()
    expect(calls.sendInput).toHaveBeenCalledWith('ls -la')
    expect(calls.recordHistory).toHaveBeenCalledWith('ls -la')
    expect(calls.showKeyError).not.toHaveBeenCalled()
  })

  it('records history with the untrimmed text', () => {
    const { t, calls } = target()

    sendCustomText({ text: '  ls  ', onSent: () => {}, ...t })

    // History is what the user typed; trimming it would make the suggestion
    // list disagree with the input box.
    expect(calls.recordHistory).toHaveBeenCalledWith('  ls  ')
    expect(calls.sendInput).toHaveBeenCalledWith('  ls  ')
  })

  it('does nothing for an empty or whitespace-only composer', () => {
    const onSent = vi.fn()
    const { t, calls } = target()

    sendCustomText({ text: '', onSent, ...t })
    sendCustomText({ text: '   ', onSent, ...t })

    expect(calls.sendInput).not.toHaveBeenCalled()
    expect(calls.recordHistory).not.toHaveBeenCalled()
    expect(onSent).not.toHaveBeenCalled()
  })

  it('appends the enter sequence after the text for the double-tap path', () => {
    const { t, calls } = target()

    sendCustomText({ text: 'ls', withEnter: true, onSent: () => {}, ...t })

    expect(calls.sendInput).toHaveBeenCalledTimes(2)
    expect(calls.sendInput).toHaveBeenNthCalledWith(1, 'ls')
    // The enter spec is parsed, not appended literally to the text.
    expect(calls.sendInput).toHaveBeenNthCalledWith(2, '\r')
  })

  it('does not append a newline by default', () => {
    const { t, calls } = target()

    sendCustomText({ text: 'ls', onSent: () => {}, ...t })

    expect(calls.sendInput).toHaveBeenCalledOnce()
  })

  it('always clears the composer, including when the enter spec is rejected', () => {
    const onSent = vi.fn()
    const { t, calls } = target()

    sendCustomText({ text: 'ls', withEnter: true, onSent, ...t })

    expect(onSent).toHaveBeenCalledOnce()
    expect(calls.showKeyError).not.toHaveBeenCalled()
  })
})
