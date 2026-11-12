/**
 * Soft-keyboard input for xterm's hidden textarea.
 *
 * xterm reads a soft keyboard through the hidden textarea: Android IMEs send a
 * placeholder `keydown` (`keyCode: 229`, `key: 'Unidentified'`) instead of a
 * real key code, and xterm then diffs the textarea in a 0ms timer
 * (`CompositionHelper._handleAnyTextareaChanges`). That diff assumes the
 * textarea only ever grows by appending — `newValue.replace(oldValue, '')` —
 * which soft keyboards violate routinely: an auto-paired quote inserts `""` and
 * parks the caret between the quotes, a correction rewrites a word in place,
 * and accepting a suggestion replaces the typed word. When the textarea no
 * longer contains the old value the diff finds nothing to remove and re-sends
 * the *whole* textarea; the textarea is only cleared on blur, so every later
 * key replays more of the buffer — the reported garble where typing `"` then
 * `d` arrives as `"""d"`.
 *
 * The earlier repair kept the textarea empty and cancelled insertions. That
 * stops the replay but fights the IME: Gboard tracks the word it is suggesting
 * against the field's contents, so a field that never receives the letters
 * drifts out of sync. The first suggestion may still work; later ones,
 * whitespace, and punctuation either duplicate the word or leave the IME
 * waiting for text that is not there (the reported "stuck until delete").
 *
 * This module takes the opposite approach: the textarea *is* the IME's buffer.
 *
 *   1. Every non-composition `input` event diffs the textarea's previous value
 *      against its new one. The diff is translated into an append-only
 *      terminal edit: backspaces for everything after the shared prefix, then
 *      the replacement's tail (`ref` -> `refactor` sends `actor`, `teh` ->
 *      `the` sends `\x7f\x7fhe`). The textarea keeps the text, so the IME can
 *      keep suggesting, replacing, and correcting against a buffer that
 *      matches what the terminal has received.
 *   2. The soft-key `keydown` and `keypress` are kept from xterm outside a
 *      composition, so its append-only diff never runs; the `input` event is
 *      kept from xterm too, because xterm would send an insertion's data a
 *      second time. A hardware key is left to xterm, and its `keypress`
 *      character is remembered so a duplicate `input` copy is not sent again.
 *   3. A key xterm handles (Enter, arrows, Tab, Escape, control combos, a
 *      hardware character) commits the IME's word: the textarea is emptied
 *      before xterm sees the key, so its own paths and a later composition
 *      start from a clean field. A line break that arrives through the IME
 *      instead is sent as `\r` and clears the mirror.
 *   4. A backspace in an empty field would be a no-op, so it is sent as one
 *      `\x7f` (forward delete as `\x1b[3~`); with text in the field the
 *      browser deletes it and the diff sends the backspace.
 *   5. Composition stays with xterm: it renders the composing text and sends
 *      the committed string from its own 0ms timer after `compositionend`.
 *      Input events are skipped while composing and until that timer has run,
 *      then the textarea is adopted as the new mirror. A composition that
 *      rewrites text *before* the composition start is still xterm's
 *      limitation (CJK candidate replacement), not something this module
 *      fixes.
 *
 * The repair only attaches on touch platforms. A hardware keyboard's
 * characters are already sent by xterm's keypress path; its `input` copy is
 * suppressed by the remembered keypress character instead of being sent twice.
 */

/** The parts of xterm's terminal this module reads or calls. */
export interface SoftKeyboardTerminal {
  readonly textarea?: SoftKeyboardTextArea | null
  readonly options: { screenReaderMode?: boolean }
  input(data: string): void
}

/** The parts of the hidden textarea this module reads or calls. */
export interface SoftKeyboardTextArea {
  value: string
  readonly selectionStart?: number | null
  readonly selectionEnd?: number | null
  addEventListener(
    type: string,
    listener: EventListener,
    options?: boolean | AddEventListenerOptions
  ): void
  removeEventListener(
    type: string,
    listener: EventListener,
    options?: boolean | AddEventListenerOptions
  ): void
}

/** The parts of an event target this module uses; a DOM element satisfies it. */
export interface SoftKeyboardTarget {
  addEventListener(
    type: string,
    listener: EventListener,
    options?: boolean | AddEventListenerOptions
  ): void
  removeEventListener(
    type: string,
    listener: EventListener,
    options?: boolean | AddEventListenerOptions
  ): void
}

/**
 * `Mobile` also catches iPadOS, which claims to be a Macintosh; touch points
 * catch tablets and desktop-mode browsers typing on glass that no user-agent
 * string admits to.
 */
const SOFT_KEYBOARD_USER_AGENT = /Android|iPhone|iPad|iPod|Mobile|Silk/i

/** Whether this browser's keyboard is expected to edit the hidden textarea. */
export function hasSoftKeyboardInput(environment: {
  userAgent?: string
  maxTouchPoints?: number
}): boolean {
  return (
    SOFT_KEYBOARD_USER_AGENT.test(environment.userAgent ?? '') ||
    (environment.maxTouchPoints ?? 0) > 0
  )
}

/**
 * The placeholder keydown of a soft keyboard. Dead keys and AltGr report
 * `keyCode: 229` on Windows as well, but they are composed by xterm's own
 * dead-key handling and must not be treated as IME input.
 */
function isSoftKeyDown(event: KeyboardEvent): boolean {
  if (event.key === 'Dead' || event.key === 'AltGraph') return false
  return event.keyCode === 229 || event.key === 'Unidentified' || event.key === 'Process'
}

/** Keys that only change modifier state: a word is not committed by them. */
const MODIFIER_KEYS = new Set([
  'Shift',
  'Control',
  'Alt',
  'AltGraph',
  'Meta',
  'OS',
  'Super',
  'Hyper',
  'CapsLock',
  'NumLock',
  'ScrollLock',
  'Fn',
  'FnLock',
  'Symbol',
  'SymbolLock',
  'Compose',
])

const DELETE_BACKWARD = new Set([
  'deleteContentBackward',
  'deleteWordBackward',
  'deleteSoftLineBackward',
  'deleteHardLineBackward',
])

const DELETE_FORWARD = new Set([
  'deleteContentForward',
  'deleteWordForward',
  'deleteSoftLineForward',
  'deleteHardLineForward',
])

/** How long after a hardware keypress its `input` copy is still recognised. */
const KEYPRESS_DEDUPE_MS = 250

/** One append-only edit: erase from the end, then type the replacement tail. */
export interface SoftKeyboardEdit {
  /** Backspaces to send before `insert`. */
  erase: number
  /** Text to type after the backspaces. */
  insert: string
}

/**
 * The edit that turns `previous` into `next` on an append-only terminal.
 *
 * The terminal cursor sits after everything already sent, so an edit at any
 * position is emulated by erasing the whole tail past the shared prefix and
 * retyping it: `teh` -> `the` is two backspaces and `he`; `teh cat` ->
 * `the cat` is six backspaces and `he cat`. Code points, not UTF-16 units, are
 * counted so a surrogate pair is never split between the erase and the
 * retype. Returns null when nothing changed.
 */
export function softKeyboardEdit(previous: string, next: string): SoftKeyboardEdit | null {
  if (previous === next) return null
  const before = [...previous]
  const after = [...next]
  let shared = 0
  while (shared < before.length && shared < after.length && before[shared] === after[shared]) {
    shared++
  }
  return {
    erase: before.length - shared,
    insert: after.slice(shared).join(''),
  }
}

/**
 * Mirror the hidden textarea's edits into `terminal.input()` on touch
 * platforms. Returns the detach function; a no-op where the soft keyboard is
 * not expected or in screen-reader mode, where xterm deliberately leaves input
 * events to the accessibility layer.
 */
export function attachSoftKeyboardInput(
  terminal: SoftKeyboardTerminal,
  container: SoftKeyboardTarget
): () => void {
  if (typeof navigator === 'undefined' || !hasSoftKeyboardInput(navigator)) return () => {}
  const textarea = terminal.textarea
  if (!textarea || terminal.options.screenReaderMode) return () => {}

  let composing = false
  let compositionFinalizing = false
  // A soft-key placeholder keydown is in flight; its character belongs to the
  // textarea, not to xterm's keypress path.
  let softKeyActive = false
  // The last character xterm sent from a real keypress, and when. Some
  // browsers still fire an `input` event for the same key; that copy is the
  // field's, not another keystroke.
  let keypressChar: string | null = null
  let keypressAt = 0
  // The textarea value the terminal has received for the current line. The
  // textarea is the IME's buffer; this is the same buffer as last mirrored.
  let mirrored = ''
  let finalizeTimer: ReturnType<typeof setTimeout> | undefined

  // Any text left in the hidden field predates this repair and was never
  // mirrored, so start from a clean line rather than sending it by accident.
  textarea.value = ''

  const clearMirror = (): void => {
    mirrored = ''
    if (textarea.value !== '') textarea.value = ''
  }

  /** Send one diff, normalising line endings and dropping the line after it. */
  const applyEdit = (edit: SoftKeyboardEdit | null): void => {
    if (!edit) return
    const insert = edit.insert.replace(/\r\n?|\n/g, '\r')
    const data = '\x7f'.repeat(edit.erase) + insert
    if (data) terminal.input(data)
    // A committed newline starts a new line in the terminal; the field's
    // buffer belongs to the old one.
    if (/[\r\n]/.test(edit.insert)) clearMirror()
  }

  const fieldIsEmpty = (): boolean => textarea.value === ''

  const handleKeyDown = (event: Event): void => {
    const key = event as KeyboardEvent
    if (isSoftKeyDown(key)) {
      softKeyActive = true
      // While a composition is in flight, xterm's CompositionHelper needs the
      // 229 keydown to finalize it and does not schedule its append-only diff
      // for it. Outside a composition the placeholder is ours alone: keeping
      // it from xterm stops that diff from sending the textarea a second time.
      if (!composing && !compositionFinalizing) event.stopImmediatePropagation()
      return
    }
    softKeyActive = false
    keypressChar = null
    // While composing, the field belongs to the IME until xterm's
    // post-composition timer has read it.
    if (composing || compositionFinalizing) return
    if (MODIFIER_KEYS.has(key.key)) return
    // A key xterm handles commits the IME's word. Drop the field's scratch
    // text before xterm samples it, so a later composition and xterm's own
    // diff start from an empty base.
    clearMirror()
  }

  const handleKeyPress = (event: Event): void => {
    const key = event as KeyboardEvent
    if (softKeyActive || isSoftKeyDown(key)) {
      // A soft key's character arrives through the textarea; letting xterm's
      // keypress path send it as well would duplicate it.
      event.stopImmediatePropagation()
      return
    }
    // A real keypress: xterm sends this character itself. Remember it so a
    // matching input event is recognised as the same keystroke.
    if (key.key.length === 1) {
      keypressChar = key.key
      keypressAt = Date.now()
    }
  }

  const handleKeyUp = (): void => {
    softKeyActive = false
  }

  const handleBeforeInput = (event: Event): void => {
    const input = event as InputEvent
    if (composing || compositionFinalizing || input.isComposing) return

    if (input.inputType === 'insertLineBreak' || input.inputType === 'insertParagraph') {
      // A soft Enter may never reach xterm as a real key code. Commit the line
      // here, as `\r`, and start the mirror on the next line. A non-cancelable
      // event cannot be stopped, so the input event below owns that newline.
      if (!input.cancelable) return
      input.preventDefault()
      terminal.input('\r')
      clearMirror()
      return
    }

    if (DELETE_BACKWARD.has(input.inputType) && fieldIsEmpty()) {
      // An empty field has nothing for the browser to delete, so no input
      // event would follow; the user still expects the key to reach the
      // terminal (e.g. to erase the prompt's last character).
      input.preventDefault()
      terminal.input('\x7f')
      return
    }

    if (DELETE_FORWARD.has(input.inputType) && fieldIsEmpty()) {
      input.preventDefault()
      terminal.input('\x1b[3~')
    }
  }

  const handleInput = (event: Event): void => {
    const input = event as InputEvent
    // xterm must not read this event: its append-only diff re-sends an
    // insertion's data and cannot describe a replacement at all.
    input.stopImmediatePropagation()
    if (composing || compositionFinalizing || input.isComposing) return

    const next = textarea.value
    if (
      keypressChar !== null &&
      input.inputType === 'insertText' &&
      input.data === keypressChar &&
      Date.now() - keypressAt < KEYPRESS_DEDUPE_MS
    ) {
      // xterm's keypress path already sent this character.
      keypressChar = null
      mirrored = next
      return
    }
    keypressChar = null
    const edit = softKeyboardEdit(mirrored, next)
    mirrored = next
    applyEdit(edit)
  }

  const handleCompositionStart = (): void => {
    composing = true
    compositionFinalizing = false
    if (finalizeTimer !== undefined) {
      clearTimeout(finalizeTimer)
      finalizeTimer = undefined
    }
  }

  /**
   * Registered on the textarea after xterm's own listener, on purpose: xterm
   * reads and sends the committed text from a 0ms timer it schedules while
   * handling this event. Ours is queued after that read, so adopting the
   * textarea as the mirror cannot lose the commit or send it twice.
   */
  const handleCompositionEnd = (): void => {
    composing = false
    compositionFinalizing = true
    finalizeTimer = setTimeout(() => {
      finalizeTimer = undefined
      compositionFinalizing = false
      if (!composing) mirrored = textarea.value
    }, 0)
  }

  const handleBlur = (): void => {
    // xterm clears the hidden field on blur (its listener runs first). The
    // line is committed; keep the mirror on the empty value it left.
    mirrored = textarea.value
    composing = false
    compositionFinalizing = false
    if (finalizeTimer !== undefined) {
      clearTimeout(finalizeTimer)
      finalizeTimer = undefined
    }
  }

  const handlePaste = (): void => {
    // xterm's paste handler (registered first) sends the clipboard text and
    // empties the field; adopt the empty value rather than diffing against
    // text that was already sent.
    mirrored = textarea.value
  }

  container.addEventListener('keydown', handleKeyDown, true)
  container.addEventListener('keypress', handleKeyPress, true)
  container.addEventListener('keyup', handleKeyUp, true)
  container.addEventListener('beforeinput', handleBeforeInput, true)
  container.addEventListener('input', handleInput, true)
  container.addEventListener('compositionstart', handleCompositionStart, true)
  textarea.addEventListener('compositionend', handleCompositionEnd)
  textarea.addEventListener('blur', handleBlur)
  textarea.addEventListener('paste', handlePaste)

  return () => {
    container.removeEventListener('keydown', handleKeyDown, true)
    container.removeEventListener('keypress', handleKeyPress, true)
    container.removeEventListener('keyup', handleKeyUp, true)
    container.removeEventListener('beforeinput', handleBeforeInput, true)
    container.removeEventListener('input', handleInput, true)
    container.removeEventListener('compositionstart', handleCompositionStart, true)
    textarea.removeEventListener('compositionend', handleCompositionEnd)
    textarea.removeEventListener('blur', handleBlur)
    textarea.removeEventListener('paste', handlePaste)
    if (finalizeTimer !== undefined) clearTimeout(finalizeTimer)
  }
}
