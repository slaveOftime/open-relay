/**
 * Soft-keyboard input repair for xterm's hidden textarea.
 *
 * xterm reads a soft keyboard through the hidden textarea: Android IMEs send a
 * placeholder `keydown` (`keyCode: 229`, `key: 'Unidentified'`) instead of a
 * real key code, and xterm then diffs the textarea in a 0ms timer
 * (`CompositionHelper._handleAnyTextareaChanges`). That diff assumes the
 * textarea only ever grows by appending — `newValue.replace(oldValue, '')` —
 * which soft keyboards violate routinely: an auto-paired quote inserts `""` and
 * parks the caret between the quotes, autocorrect rewrites a word in place.
 * When the textarea no longer contains the old value, the diff finds nothing to
 * remove and re-sends the *whole* textarea. The textarea is only cleared on
 * blur, so every later key replays more of the buffer — the reported garble
 * where typing `"` then `d` arrives as `"""d"`.
 *
 * The repair keeps the textarea empty instead:
 *
 *   1. A `beforeinput` insertion while a soft-key keydown is in flight is
 *      cancelled and sent once through `terminal.input()`. xterm's diff timer
 *      then compares an empty textarea against an empty textarea and sends
 *      nothing.
 *   2. A keydown sweeps residue a keyboard left behind silently (the
 *      auto-paired quote never fires an event) before xterm samples the
 *      textarea for that key, so the replay has no base to grow from.
 *   3. Composition stays with xterm: it reads the textarea from a 0ms timer
 *      after `compositionend`, so the sweep is skipped while composing and the
 *      post-composition clear is queued from a listener registered after
 *      xterm's — i.e. after xterm's timer.
 *
 * The repair only attaches on touch platforms. A hardware keyboard's
 * characters are already sent by xterm's keypress path, and cancelling the
 * insertion that accompanies them would send each character twice; the
 * interception therefore also requires the soft-key placeholder keydown and
 * stands down as soon as a keypress is seen.
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

export interface SoftKeyboardInsertion {
  /** `InputEvent.inputType` of the pending edit. */
  inputType: string
  /** `InputEvent.data`: the text the browser is about to insert. */
  data: string | null
  /** `InputEvent.isComposing`: the edit belongs to an active composition. */
  isComposing: boolean
  /**
   * `InputEvent.cancelable`: only an edit we can actually suppress may be
   * taken over, or the native insertion and our send would both land.
   */
  cancelable: boolean
  /** A soft-key placeholder keydown is in flight (see `isSoftKeyDown`). */
  softKeyPending: boolean
  /** A composition ended and xterm's deferred read of the textarea has not run. */
  compositionFinalizing: boolean
}

/**
 * The text to send in place of a native insertion, or null to leave the event
 * alone (xterm's own paths then own it).
 */
export function softKeyboardText(event: SoftKeyboardInsertion): string | null {
  if (!event.cancelable || !event.softKeyPending) return null
  if (event.isComposing || event.compositionFinalizing) return null
  if (event.inputType !== 'insertText' && event.inputType !== 'insertReplacementText') return null
  return event.data || null
}

/**
 * Keep the hidden textarea empty while a soft keyboard types into it, sending
 * every insertion exactly once. Returns the detach function; a no-op on
 * platforms without a soft keyboard and in screen-reader mode, where xterm
 * deliberately leaves input events to the accessibility layer.
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
  let softKeyPending = false
  let finalizeTimer: ReturnType<typeof setTimeout> | undefined

  const clearResidue = (): void => {
    textarea.value = ''
  }

  const handleKeyDown = (event: Event): void => {
    // While composing, and until xterm's post-composition timer has read the
    // textarea, the contents belong to the IME and are off limits.
    if (composing || compositionFinalizing) return
    // Residue a keyboard left behind silently (an auto-paired quote fires no
    // event at all) would become the base of this key's diff, and a diff that
    // cannot find its old value replays the whole buffer. Drop it now, before
    // xterm samples the textarea for this key.
    clearResidue()
    softKeyPending = isSoftKeyDown(event as KeyboardEvent)
  }

  const handleKeyPress = (): void => {
    // A keypress means the browser delivered this key through the normal
    // keyboard path, which xterm sends and then cancels the input event for —
    // never send it a second time ourselves.
    softKeyPending = false
  }

  const handleKeyUp = (): void => {
    softKeyPending = false
  }

  const handleBeforeInput = (event: Event): void => {
    const input = event as InputEvent
    const data = softKeyboardText({
      inputType: input.inputType,
      data: input.data,
      isComposing: input.isComposing,
      cancelable: input.cancelable,
      softKeyPending,
      compositionFinalizing,
    })
    if (data === null) return
    // Cancel the native insertion so the textarea stays empty; xterm's diff
    // timer then reads no change and sends nothing.
    input.preventDefault()
    terminal.input(data)
  }

  const handleCompositionStart = (): void => {
    composing = true
  }

  /**
   * Registered on the textarea, after xterm's own listener, on purpose: xterm
   * reads the textarea from a 0ms timer it schedules while handling this event,
   * and an earlier listener would queue its clear *before* that read — losing
   * the composed text. Everything else is captured on the container, ahead of
   * xterm's textarea handlers.
   */
  const handleCompositionEnd = (): void => {
    composing = false
    compositionFinalizing = true
    finalizeTimer = setTimeout(() => {
      finalizeTimer = undefined
      compositionFinalizing = false
      if (!composing) clearResidue()
    }, 0)
  }

  container.addEventListener('keydown', handleKeyDown, true)
  container.addEventListener('keypress', handleKeyPress, true)
  container.addEventListener('keyup', handleKeyUp, true)
  container.addEventListener('beforeinput', handleBeforeInput, true)
  container.addEventListener('compositionstart', handleCompositionStart, true)
  textarea.addEventListener('compositionend', handleCompositionEnd)

  return () => {
    container.removeEventListener('keydown', handleKeyDown, true)
    container.removeEventListener('keypress', handleKeyPress, true)
    container.removeEventListener('keyup', handleKeyUp, true)
    container.removeEventListener('beforeinput', handleBeforeInput, true)
    container.removeEventListener('compositionstart', handleCompositionStart, true)
    textarea.removeEventListener('compositionend', handleCompositionEnd)
    if (finalizeTimer !== undefined) clearTimeout(finalizeTimer)
  }
}
