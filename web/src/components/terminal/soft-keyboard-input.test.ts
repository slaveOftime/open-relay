import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import {
  attachSoftKeyboardInput,
  hasSoftKeyboardInput,
  softKeyboardEdit,
  type SoftKeyboardTarget,
  type SoftKeyboardTerminal,
  type SoftKeyboardTextArea,
} from './soft-keyboard-input'

/**
 * The suite runs in the `node` environment (see vite.config.ts), so it builds
 * the minimum terminal/textarea/event shapes the module touches and drives its
 * listeners by hand — the same approach as `xterm-theme.test.ts`.
 */

const ANDROID_UA =
  'Mozilla/5.0 (Linux; Android 13; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36'
const DESKTOP_UA =
  'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36'

class FakeEvent {
  readonly type: string
  defaultPrevented = false
  immediatePropagationStopped = false

  constructor(type: string) {
    this.type = type
  }

  preventDefault(): void {
    this.defaultPrevented = true
  }

  stopImmediatePropagation(): void {
    this.immediatePropagationStopped = true
  }
}

/** A soft-key keydown by default: Android's `keyCode: 229` placeholder. */
class FakeKeyboardEvent extends FakeEvent {
  readonly key: string
  readonly keyCode: number
  readonly charCode: number

  constructor(init: { type: string; key?: string; keyCode?: number; charCode?: number }) {
    super(init.type)
    this.key = init.key ?? 'Unidentified'
    this.keyCode = init.keyCode ?? 229
    this.charCode = init.charCode ?? 0
  }
}

class FakeInputEvent extends FakeEvent {
  readonly inputType: string
  readonly data: string | null
  readonly isComposing: boolean
  readonly cancelable: boolean

  constructor(init: {
    type?: string
    inputType?: string
    data?: string | null
    isComposing?: boolean
    cancelable?: boolean
  }) {
    super(init.type ?? 'input')
    this.inputType = init.inputType ?? 'insertText'
    this.data = init.data ?? null
    this.isComposing = init.isComposing ?? false
    this.cancelable = init.cancelable ?? true
  }
}

interface Registration {
  listener: EventListener
  capture: boolean
}

class FakeEventTarget implements SoftKeyboardTarget {
  private readonly registrations = new Map<string, Registration[]>()

  addEventListener(
    type: string,
    listener: EventListener,
    options?: boolean | AddEventListenerOptions
  ): void {
    const capture = options === true || (typeof options === 'object' && Boolean(options.capture))
    const bucket = this.registrations.get(type) ?? []
    bucket.push({ listener, capture })
    this.registrations.set(type, bucket)
  }

  removeEventListener(
    type: string,
    listener: EventListener,
    options?: boolean | AddEventListenerOptions
  ): void {
    const capture = options === true || (typeof options === 'object' && Boolean(options.capture))
    const bucket = this.registrations.get(type) ?? []
    this.registrations.set(
      type,
      bucket.filter((entry) => entry.listener !== listener || entry.capture !== capture)
    )
  }

  /** Deliver `event` to this target's listeners, in registration order. */
  dispatch(event: FakeEvent): void {
    for (const { listener } of [...(this.registrations.get(event.type) ?? [])]) {
      if (event.immediatePropagationStopped) break
      listener(event as unknown as Event)
    }
  }

  registrationsFor(type: string): Registration[] {
    return this.registrations.get(type) ?? []
  }
}

class FakeTextarea extends FakeEventTarget implements SoftKeyboardTextArea {
  selectionStart = 0
  selectionEnd = 0
  private current = ''

  get value(): string {
    return this.current
  }

  set value(next: string) {
    this.current = next
    this.selectionStart = next.length
    this.selectionEnd = next.length
  }

  /** Type at the caret, as a soft key does. */
  type(text: string): void {
    const start = this.selectionStart
    const end = this.selectionEnd
    this.value = this.current.slice(0, start) + text + this.current.slice(end)
    // A real textarea leaves the caret after the inserted text, not at the
    // end of the value (the auto-pair case depends on that).
    const caret = start + text.length
    this.selectionStart = caret
    this.selectionEnd = caret
  }

  /** Replace the whole value, as an IME suggestion or autocorrection does. */
  replace(text: string): void {
    this.value = text
  }
}

class FakeTerminal implements SoftKeyboardTerminal {
  readonly inputs: string[] = []
  options: { screenReaderMode?: boolean } = {}
  readonly textarea = new FakeTextarea()

  input(data: string): void {
    this.inputs.push(data)
  }
}

interface Fixture {
  terminal: FakeTerminal
  container: FakeEventTarget
  detach: () => void
}

function attach(): Fixture {
  const terminal = new FakeTerminal()
  const container = new FakeEventTarget()
  return { terminal, container, detach: attachSoftKeyboardInput(terminal, container) }
}

function imeKeyDown(): FakeKeyboardEvent {
  return new FakeKeyboardEvent({ type: 'keydown' })
}

function hardwareKeyDown(key: string, keyCode: number): FakeKeyboardEvent {
  return new FakeKeyboardEvent({ type: 'keydown', key, keyCode })
}

function insertText(data: string, inputType = 'insertText'): FakeInputEvent {
  return new FakeInputEvent({ inputType, data })
}

function composition(type: string): FakeEvent {
  return new FakeEvent(type)
}

/** One soft key: placeholder keydown, the IME's edit, the input event. */
function softKey(
  fixture: Fixture,
  text: string,
  inputType = 'insertText',
  data: string | null = text
): FakeInputEvent {
  fixture.container.dispatch(imeKeyDown())
  fixture.terminal.textarea.type(text)
  const input = new FakeInputEvent({ inputType, data })
  fixture.container.dispatch(input)
  return input
}

/** An IME replacement: the field's value is rewritten in place. */
function replaceText(
  fixture: Fixture,
  value: string,
  inputType = 'insertReplacementText'
): FakeInputEvent {
  fixture.terminal.textarea.replace(value)
  const input = new FakeInputEvent({ inputType, data: value })
  fixture.container.dispatch(input)
  return input
}

afterEach(() => {
  vi.unstubAllGlobals()
  vi.useRealTimers()
})

describe('hasSoftKeyboardInput', () => {
  it('recognises phones and tablets by user agent', () => {
    expect(hasSoftKeyboardInput({ userAgent: ANDROID_UA })).toBe(true)
    expect(
      hasSoftKeyboardInput({
        userAgent: 'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) Mobile/15E148',
      })
    ).toBe(true)
  })

  it('recognises an iPad that claims to be a Macintosh by its touch points', () => {
    expect(
      hasSoftKeyboardInput({
        userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Safari/605.1.15',
        maxTouchPoints: 5,
      })
    ).toBe(true)
  })

  it('stays off a desktop browser with no touch screen', () => {
    expect(hasSoftKeyboardInput({ userAgent: DESKTOP_UA, maxTouchPoints: 0 })).toBe(false)
    expect(hasSoftKeyboardInput({})).toBe(false)
  })
})

describe('softKeyboardEdit', () => {
  it('sends only what was appended', () => {
    expect(softKeyboardEdit('', 'd')).toEqual({ erase: 0, insert: 'd' })
    expect(softKeyboardEdit('ab', 'abc')).toEqual({ erase: 0, insert: 'c' })
  })

  it('sends nothing when the value did not change', () => {
    expect(softKeyboardEdit('go', 'go')).toBeNull()
    expect(softKeyboardEdit('', '')).toBeNull()
  })

  it('extends a typed word with only the suggestion suffix', () => {
    // `ref` -> `refactor`: the reported `refrefactor` came from sending the
    // whole replacement on top of the word already sent.
    expect(softKeyboardEdit('ref', 'refactor')).toEqual({ erase: 0, insert: 'actor' })
  })

  it('erases only the differing tail of a correction', () => {
    expect(softKeyboardEdit('teh', 'the')).toEqual({ erase: 2, insert: 'he' })
    expect(softKeyboardEdit('brpwn', 'brown')).toEqual({ erase: 3, insert: 'own' })
  })

  it('erases past a shared suffix before retyping it', () => {
    // A correction before a committed space cannot reach the changed word by
    // backspacing only the changed letters: the space and the tail sit after
    // it, so they are erased and retyped too.
    expect(softKeyboardEdit('teh ', 'the ')).toEqual({ erase: 3, insert: 'he ' })
    expect(softKeyboardEdit('teh cat', 'the cat')).toEqual({ erase: 6, insert: 'he cat' })
  })

  it('turns a deletion into backspaces', () => {
    expect(softKeyboardEdit('abc', 'ab')).toEqual({ erase: 1, insert: '' })
    expect(softKeyboardEdit('abc', '')).toEqual({ erase: 3, insert: '' })
  })

  it('counts code points, never splitting a surrogate pair', () => {
    expect(softKeyboardEdit('\u{1f600}x', '\u{1f600}y')).toEqual({ erase: 1, insert: 'y' })
    expect(softKeyboardEdit('\u{1f600}x', '\u{1f600}')).toEqual({ erase: 1, insert: '' })
  })
})

describe('attachSoftKeyboardInput', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    vi.stubGlobal('navigator', { userAgent: ANDROID_UA, maxTouchPoints: 5 })
  })

  it('lets the IME fill the textarea and sends each edit once', () => {
    const fixture = attach()
    const { terminal, container } = fixture

    softKey(fixture, 'd')
    softKey(fixture, 'e')

    expect(terminal.inputs).toEqual(['d', 'e'])
    expect(terminal.textarea.value).toBe('de')
    // The field is the IME's buffer: it keeps the text it typed.
    expect(container.registrationsFor('input').map((r) => r.capture)).toEqual([true])
  })

  it('keeps the soft-key keydown and keypress from xterm', () => {
    const fixture = attach()
    const keydown = imeKeyDown()
    const keypress = new FakeKeyboardEvent({ type: 'keypress', key: 'd', charCode: 100 })

    fixture.container.dispatch(keydown)
    fixture.container.dispatch(keypress)

    // xterm's append-only diff and its keypress path must both stand down.
    expect(keydown.immediatePropagationStopped).toBe(true)
    expect(keypress.immediatePropagationStopped).toBe(true)
  })

  it('leaves a soft keydown to xterm while a composition is in flight', () => {
    const fixture = attach()
    fixture.container.dispatch(composition('compositionstart'))

    // xterm's CompositionHelper needs the placeholder to finalize the
    // composition, and does not schedule its textarea diff for it.
    const keydown = imeKeyDown()
    fixture.container.dispatch(keydown)

    expect(keydown.immediatePropagationStopped).toBe(false)
  })

  it('keeps the input event from xterm, which would send it a second time', () => {
    const fixture = attach()
    const insertion = softKey(fixture, 'd')

    expect(insertion.immediatePropagationStopped).toBe(true)
    expect(fixture.terminal.inputs).toEqual(['d'])
  })

  it('turns a tapped suggestion into only the erased-and-retyped delta', () => {
    const fixture = attach()

    for (const ch of 'ref') softKey(fixture, ch)
    const replacement = replaceText(fixture, 'refactor')

    // `ref` was already sent; the suggestion adds only `actor` — never the
    // `refrefactor` the reported behaviour produced.
    expect(fixture.terminal.inputs).toEqual(['r', 'e', 'f', 'actor'])
    expect(replacement.immediatePropagationStopped).toBe(true)
    expect(fixture.terminal.textarea.value).toBe('refactor')
  })

  it('sends nothing when space accepts the suggestion as typed', () => {
    const fixture = attach()

    for (const ch of 'go') softKey(fixture, ch)
    replaceText(fixture, 'go')
    softKey(fixture, ' ')

    // The reported `gogo`: the accepted `go` must not be sent a second time
    // before the space that accepted it.
    expect(fixture.terminal.inputs).toEqual(['g', 'o', ' '])
    expect(fixture.terminal.textarea.value).toBe('go ')
  })

  it('sends a comma after a suggestion exactly once', () => {
    const fixture = attach()

    for (const ch of 'ref') softKey(fixture, ch)
    replaceText(fixture, 'refactor')
    softKey(fixture, ',')

    expect(fixture.terminal.inputs).toEqual(['r', 'e', 'f', 'actor', ','])
    expect(fixture.terminal.textarea.value).toBe('refactor,')
  })

  it('erases and retypes an autocorrection before a committed space', () => {
    const fixture = attach()

    for (const ch of 'teh ') softKey(fixture, ch)
    replaceText(fixture, 'the ')

    expect(fixture.terminal.inputs).toEqual(['t', 'e', 'h', ' ', '\x7f\x7f\x7fhe '])
    expect(fixture.terminal.textarea.value).toBe('the ')
  })

  it('takes a suggestion tap without a keydown', () => {
    const fixture = attach()

    for (const ch of 'ref') softKey(fixture, ch)
    // No keydown precedes a tap on the suggestion strip.
    replaceText(fixture, 'refactor')
    softKey(fixture, 'x')

    expect(fixture.terminal.inputs).toEqual(['r', 'e', 'f', 'actor', 'x'])
    expect(fixture.terminal.textarea.value).toBe('refactorx')
  })

  it('sends a silent textarea change once, not on every later key', () => {
    const fixture = attach()
    // What an auto-paired quote looks like: the pair appears in the field
    // without an input event. The next diff picks it up exactly once; the old
    // bug re-sent the whole buffer on every later key.
    fixture.terminal.textarea.value = '""'
    fixture.terminal.textarea.selectionStart = 1
    fixture.terminal.textarea.selectionEnd = 1

    softKey(fixture, 'd')
    softKey(fixture, 'x')

    // The auto-pair left the caret before the closing quote, so the next key
    // is a minimal edit: erase the quote, type the character and the quote
    // back — not a replay of the line.
    expect(fixture.terminal.inputs).toEqual(['"d"', '\x7fx"'])
    expect(fixture.terminal.textarea.value).toBe('"dx"')
  })

  it('sends one backspace when the field holds text to delete', () => {
    const fixture = attach()

    for (const ch of 'abc') softKey(fixture, ch)
    const before = new FakeInputEvent({
      type: 'beforeinput',
      inputType: 'deleteContentBackward',
      data: null,
    })
    fixture.container.dispatch(before)
    expect(before.defaultPrevented).toBe(false)

    // The browser applies the deletion, then reports it.
    fixture.terminal.textarea.value = 'ab'
    fixture.container.dispatch(
      new FakeInputEvent({ inputType: 'deleteContentBackward', data: null })
    )

    expect(fixture.terminal.inputs).toEqual(['a', 'b', 'c', '\x7f'])
    expect(fixture.terminal.textarea.value).toBe('ab')
  })

  it('sends a backspace when an empty field has nothing to delete', () => {
    const fixture = attach()
    const deletion = new FakeInputEvent({
      type: 'beforeinput',
      inputType: 'deleteContentBackward',
      data: null,
    })

    fixture.container.dispatch(deletion)

    expect(deletion.defaultPrevented).toBe(true)
    expect(fixture.terminal.inputs).toEqual(['\x7f'])
  })

  it('leaves a backspace in a non-empty field to the browser', () => {
    const fixture = attach()

    for (const ch of 'ab') softKey(fixture, ch)
    // The caret is at the start of a non-empty field: there is nothing before
    // it to delete, and sending a backspace would erase the terminal's last
    // character, which the field still contains.
    fixture.terminal.textarea.selectionStart = 0
    fixture.terminal.textarea.selectionEnd = 0
    const deletion = new FakeInputEvent({
      type: 'beforeinput',
      inputType: 'deleteContentBackward',
      data: null,
    })
    fixture.container.dispatch(deletion)

    expect(deletion.defaultPrevented).toBe(false)
    expect(fixture.terminal.inputs).toEqual(['a', 'b'])
  })

  it('sends a forward delete when the field is empty', () => {
    const fixture = attach()
    const deletion = new FakeInputEvent({
      type: 'beforeinput',
      inputType: 'deleteContentForward',
      data: null,
    })

    fixture.container.dispatch(deletion)

    expect(deletion.defaultPrevented).toBe(true)
    expect(fixture.terminal.inputs).toEqual(['\x1b[3~'])
  })

  it('leaves a forward delete in a non-empty field to the browser', () => {
    const fixture = attach()

    for (const ch of 'ab') softKey(fixture, ch)
    const deletion = new FakeInputEvent({
      type: 'beforeinput',
      inputType: 'deleteContentForward',
      data: null,
    })
    fixture.container.dispatch(deletion)

    expect(deletion.defaultPrevented).toBe(false)
    expect(fixture.terminal.inputs).toEqual(['a', 'b'])
  })

  it('sends a soft Enter as carriage return and clears the mirror', () => {
    const fixture = attach()

    for (const ch of 'go') softKey(fixture, ch)
    const lineBreak = new FakeInputEvent({
      type: 'beforeinput',
      inputType: 'insertLineBreak',
      data: null,
    })
    fixture.container.dispatch(lineBreak)

    expect(lineBreak.defaultPrevented).toBe(true)
    expect(fixture.terminal.inputs).toEqual(['g', 'o', '\r'])
    expect(fixture.terminal.textarea.value).toBe('')

    softKey(fixture, 'r')
    expect(fixture.terminal.inputs).toEqual(['g', 'o', '\r', 'r'])
  })

  it('normalises a newline that arrives in an input event', () => {
    const fixture = attach()

    softKey(fixture, 'a\nb')

    expect(fixture.terminal.inputs).toEqual(['a\rb'])
    expect(fixture.terminal.textarea.value).toBe('')

    softKey(fixture, 'c')
    expect(fixture.terminal.inputs).toEqual(['a\rb', 'c'])
  })

  it('starts a fresh word after a key xterm handles, so a replacement cannot reach back', () => {
    const fixture = attach()

    for (const ch of 'go') softKey(fixture, ch)
    fixture.container.dispatch(hardwareKeyDown('Enter', 13))
    expect(fixture.terminal.textarea.value).toBe('')

    for (const ch of 'ref') softKey(fixture, ch)
    replaceText(fixture, 'refactor')

    expect(fixture.terminal.inputs).toEqual(['g', 'o', 'r', 'e', 'f', 'actor'])
  })

  it('keeps the IME buffer through a modifier keydown', () => {
    const fixture = attach()

    for (const ch of 're') softKey(fixture, ch)
    fixture.container.dispatch(hardwareKeyDown('Shift', 16))
    expect(fixture.terminal.textarea.value).toBe('re')

    softKey(fixture, 'f')
    expect(fixture.terminal.inputs).toEqual(['r', 'e', 'f'])
  })

  it('does not send the input copy of a character xterm already sent from keypress', () => {
    const fixture = attach()

    fixture.container.dispatch(hardwareKeyDown('a', 65))
    const keypress = new FakeKeyboardEvent({
      type: 'keypress',
      key: 'a',
      keyCode: 97,
      charCode: 97,
    })
    fixture.container.dispatch(keypress)
    expect(keypress.immediatePropagationStopped).toBe(false)

    // Some browsers still fire the field's input event for the same key.
    fixture.terminal.textarea.type('a')
    fixture.container.dispatch(insertText('a'))
    expect(fixture.terminal.inputs).toEqual([])

    // The mirror is still aligned: the next IME character is the only send.
    softKey(fixture, 'b')
    expect(fixture.terminal.inputs).toEqual(['b'])
  })

  it('leaves a composition to xterm and adopts its committed text afterwards', () => {
    const terminal = new FakeTerminal()
    const container = new FakeEventTarget()
    let start = 0
    // Model xterm's own listeners and its 0ms read of the committed text:
    // registered while the terminal opened, before the repair attaches. xterm
    // records the field length at compositionstart and sends only the text
    // after it, so a word already in the field is not replayed.
    terminal.textarea.addEventListener('compositionstart', () => {
      start = terminal.textarea.value.length
    })
    terminal.textarea.addEventListener('compositionend', () => {
      setTimeout(() => {
        const committed = terminal.textarea.value.substring(start)
        if (committed) terminal.inputs.push(committed)
      }, 0)
    })
    attachSoftKeyboardInput(terminal, container)
    const fixture = { terminal, container, detach: () => {} }

    for (const ch of 'hi') softKey(fixture, ch)
    expect(terminal.inputs).toEqual(['h', 'i'])

    // The IME composes after the word already typed: the field holds both.
    container.dispatch(composition('compositionstart'))
    terminal.textarea.dispatch(composition('compositionstart'))
    terminal.textarea.value = 'hi你好'
    container.dispatch(insertText('hi你好', 'insertCompositionText'))
    expect(terminal.inputs).toEqual(['h', 'i'])

    terminal.textarea.dispatch(composition('compositionend'))
    // Nothing is cleared while xterm's read is still pending.
    expect(terminal.textarea.value).toBe('hi你好')

    vi.runAllTimers()

    expect(terminal.inputs).toEqual(['h', 'i', '你好'])
    expect(terminal.textarea.value).toBe('hi你好')

    // The commit is xterm's send; the next soft key adds only its own text.
    container.dispatch(imeKeyDown())
    terminal.textarea.type('x')
    container.dispatch(insertText('x'))
    expect(terminal.inputs).toEqual(['h', 'i', '你好', 'x'])
    expect(terminal.textarea.value).toBe('hi你好x')
  })

  it('does not touch the field while a composition is in progress', () => {
    const fixture = attach()
    fixture.container.dispatch(composition('compositionstart'))
    fixture.terminal.textarea.value = 'ni'

    const update = insertText('ni', 'insertCompositionText')
    fixture.container.dispatch(update)

    expect(fixture.terminal.textarea.value).toBe('ni')
    expect(fixture.terminal.inputs).toEqual([])
    expect(update.defaultPrevented).toBe(false)
  })

  it('adopts the empty field xterm leaves on blur', () => {
    const terminal = new FakeTerminal()
    const container = new FakeEventTarget()
    terminal.textarea.addEventListener('blur', () => {
      terminal.textarea.value = ''
    })
    attachSoftKeyboardInput(terminal, container)
    terminal.textarea.value = 'abc'

    terminal.textarea.dispatch(composition('blur'))
    expect(terminal.textarea.value).toBe('')

    container.dispatch(imeKeyDown())
    terminal.textarea.type('a')
    container.dispatch(insertText('a'))

    expect(terminal.inputs).toEqual(['a'])
  })

  it('adopts the empty field xterm leaves after a paste', () => {
    const terminal = new FakeTerminal()
    const container = new FakeEventTarget()
    terminal.textarea.addEventListener('paste', () => {
      terminal.inputs.push('pasted')
      terminal.textarea.value = ''
    })
    attachSoftKeyboardInput(terminal, container)
    terminal.textarea.value = 'abc'

    terminal.textarea.dispatch(composition('paste'))
    expect(terminal.textarea.value).toBe('')

    container.dispatch(imeKeyDown())
    terminal.textarea.type('a')
    container.dispatch(insertText('a'))

    expect(terminal.inputs).toEqual(['pasted', 'a'])
  })

  it('captures on the container so it runs before xterm’s textarea handlers', () => {
    const { terminal, container } = attach()

    expect(container.registrationsFor('keydown').map((r) => r.capture)).toEqual([true])
    expect(container.registrationsFor('keypress').map((r) => r.capture)).toEqual([true])
    expect(container.registrationsFor('input').map((r) => r.capture)).toEqual([true])
    expect(container.registrationsFor('beforeinput').map((r) => r.capture)).toEqual([true])
    expect(container.registrationsFor('compositionstart').map((r) => r.capture)).toEqual([true])
    // The composition read must not be captured: only registration order can
    // put it after xterm's own listener on the same element.
    expect(terminal.textarea.registrationsFor('compositionend').map((r) => r.capture)).toEqual([
      false,
    ])
  })

  it('detaches every listener and cancels a pending composition read', () => {
    const fixture = attach()
    const { terminal, container } = fixture
    const textarea = terminal.textarea
    textarea.value = '你好'
    textarea.dispatch(composition('compositionend'))

    fixture.detach()
    vi.runAllTimers()
    container.dispatch(imeKeyDown())
    textarea.type('d')
    container.dispatch(insertText('d'))

    expect(textarea.value).toBe('你好d')
    expect(terminal.inputs).toEqual([])
    expect(container.registrationsFor('keydown')).toEqual([])
    expect(container.registrationsFor('input')).toEqual([])
    expect(container.registrationsFor('compositionstart')).toEqual([])
    expect(textarea.registrationsFor('compositionend')).toEqual([])
    expect(textarea.registrationsFor('blur')).toEqual([])
    expect(textarea.registrationsFor('paste')).toEqual([])
  })

  it('never attaches on a platform without a soft keyboard', () => {
    vi.stubGlobal('navigator', { userAgent: DESKTOP_UA, maxTouchPoints: 0 })
    const fixture = attach()
    fixture.terminal.textarea.value = '""'

    softKey(fixture, 'd')

    expect(fixture.container.registrationsFor('keydown')).toEqual([])
    expect(fixture.terminal.textarea.value).toBe('""d')
    expect(fixture.terminal.inputs).toEqual([])
    fixture.detach()
  })

  it('never attaches in screen-reader mode', () => {
    const terminal = new FakeTerminal()
    terminal.options.screenReaderMode = true
    const container = new FakeEventTarget()

    attachSoftKeyboardInput(terminal, container)

    expect(container.registrationsFor('keydown')).toEqual([])
  })
})
