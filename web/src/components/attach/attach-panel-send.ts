/**
 * The attach panel's send path: text, a single key spec, or a list of specs.
 *
 * Kept out of the component because it is the one part of the panel with real
 * logic in it — which strings are accepted, what a bad one does, and what gets
 * cleared after a send — and because it can be exercised without rendering
 * anything.
 */

import { parseKeyInputSpecs, parseKeySpec, splitKeyInput } from '@/utils/key-input'

export interface AttachSendTarget {
  sendInput: (data: string) => void
  showKeyError: (message: string) => void
  /** Called with the trimmed text a send accepted, for input history. */
  recordHistory: (text: string) => void
}

/** Reports a rejected spec without losing the error's own message. */
function reportKeyError(error: unknown, target: AttachSendTarget): void {
  target.showKeyError(error instanceof Error ? error.message : 'invalid key spec')
}

export function sendKeySpec(spec: string, target: AttachSendTarget): void {
  try {
    target.sendInput(parseKeySpec(spec))
  } catch (error) {
    reportKeyError(error, target)
  }
}

export interface CustomKeysSendOptions extends AttachSendTarget {
  text: string
  /** Clears the custom-keys composer, only after every spec was accepted. */
  onSent: () => void
}

export function sendCustomKeys({
  text,
  onSent,
  sendInput,
  showKeyError,
  recordHistory,
}: CustomKeysSendOptions): void {
  const specs = splitKeyInput(text.trim())
  if (specs.length === 0) return
  try {
    const parsed = parseKeyInputSpecs(specs)
    for (const data of parsed) {
      sendInput(data)
    }
    onSent()
  } catch (error) {
    reportKeyError(error, { sendInput, showKeyError, recordHistory })
  }
}

export interface CustomSendOptions extends AttachSendTarget {
  text: string
  /** Append a newline after the text, the double-tap-to-send behaviour. */
  withEnter?: boolean
  /** Clears the composer and any staged image previews after a send. */
  onSent: () => void
}

export function sendCustomText({
  text,
  withEnter,
  onSent,
  sendInput,
  showKeyError,
  recordHistory,
}: CustomSendOptions): void {
  const trimmed = text.trim()
  if (!trimmed) return
  recordHistory(text)
  sendInput(text)
  if (withEnter) {
    sendKeySpec('enter', { sendInput, showKeyError, recordHistory })
  }
  onSent()
}
