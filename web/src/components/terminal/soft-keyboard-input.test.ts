import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import {
  attachSoftKeyboardInput,
  hasSoftKeyboardInput,
  softKeyboardText,
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

  constructor(type: string) {
    this.type = type
  }

  preventDefault(): void {
    this.defaultPrevented = true
  }
}

/** A soft-key keydown by default: Android's `keyCode: 229` placeholder. */
class FakeKeyboardEvent extends FakeEvent {
  readonly key: string
  readonly keyCode: number

  constructor(init: { type: string; key?: string; keyCode?: number }) {
    super(init.type)
    this.key = init.key ?? 'Unidentified'
    this.keyCode = init.keyCode ?? 229
  }
}

class FakeInputEvent extends FakeEvent {
  readonly inputType: string
  readonly data: string | null
  readonly isComposing: boolean
  readonly cancelable: boolean

  constructor(init: {
    inputType?: string
    data?: string | null
    isComposing?: boolean
    cancelable?: boolean
  }) {
    super('beforeinput')
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
      listener(event as unknown as Event)
    }
  }

  registrationsFor(type: string): Registration[] {
    return this.registrations.get(type) ?? []
  }
}

class FakeTextarea extends FakeEventTarget implements SoftKeyboardTextArea {
  value = ''
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

describe('softKeyboardText', () => {
  const pending = {
    inputType: 'insertText',
    data: 'd',
    isComposing: false,
    cancelable: true,
    softKeyPending: true,
    compositionFinalizing: false,
  }

  it('sends the incoming text for a soft key', () => {
    expect(softKeyboardText(pending)).toBe('d')
  })

  it('sends a replacement edit too, so xterm never has to diff one', () => {
    expect(softKeyboardText({ ...pending, inputType: 'insertReplacementText' })).toBe('d')
  })

  it('leaves a key the native keyboard path already sent alone', () => {
    expect(softKeyboardText({ ...pending, softKeyPending: false })).toBeNull()
  })

  it('leaves composition and the read after it to xterm', () => {
    expect(softKeyboardText({ ...pending, isComposing: true })).toBeNull()
    expect(softKeyboardText({ ...pending, compositionFinalizing: true })).toBeNull()
  })

  it('ignores edits that insert no text', () => {
    expect(softKeyboardText({ ...pending, inputType: 'insertCompositionText' })).toBeNull()
    expect(softKeyboardText({ ...pending, inputType: 'deleteContentBackward' })).toBeNull()
    expect(softKeyboardText({ ...pending, data: null })).toBeNull()
  })

  it('never takes over an edit it cannot cancel', () => {
    expect(softKeyboardText({ ...pending, cancelable: false })).toBeNull()
  })
})

describe('attachSoftKeyboardInput', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    vi.stubGlobal('navigator', { userAgent: ANDROID_UA, maxTouchPoints: 5 })
  })

  it('sends a soft-key insertion once and leaves the textarea empty', () => {
    const { terminal, container } = attach()

    container.dispatch(imeKeyDown())
    const insertion = insertText('d')
    container.dispatch(insertion)

    expect(terminal.inputs).toEqual(['d'])
    expect(insertion.defaultPrevented).toBe(true)
    expect(terminal.textarea.value).toBe('')
  })

  it('drops residue a keyboard left behind silently before the next key diff', () => {
    const { terminal, container } = attach()
    // What an auto-paired quote looks like to xterm: the pair appears in the
    // textarea without any event, so a later diff can no longer find its old
    // value and would replay the whole buffer (the `"""d"` report).
    terminal.textarea.value = '""'

    container.dispatch(imeKeyDown())

    expect(terminal.textarea.value).toBe('')
  })

  it('stands down once a keypress shows the native keyboard path owns the key', () => {
    const { terminal, container } = attach()

    container.dispatch(imeKeyDown())
    container.dispatch(composition('keypress'))
    const insertion = insertText('z')
    container.dispatch(insertion)

    expect(terminal.inputs).toEqual([])
    expect(insertion.defaultPrevented).toBe(false)
  })

  it('leaves a hardware key’s insertion to the keypress path', () => {
    const { terminal, container } = attach()

    container.dispatch(hardwareKeyDown('a', 65))
    const insertion = insertText('a')
    container.dispatch(insertion)

    expect(terminal.inputs).toEqual([])
    expect(insertion.defaultPrevented).toBe(false)
  })

  it('leaves a non-cancelable insertion to xterm’s own diff', () => {
    const { terminal, container } = attach()

    container.dispatch(imeKeyDown())
    const insertion = new FakeInputEvent({ data: 'd', cancelable: false })
    container.dispatch(insertion)

    // The browser inserted it natively; xterm's diff timer sends it.
    expect(insertion.defaultPrevented).toBe(false)
    expect(terminal.inputs).toEqual([])
  })

  it('never touches the textarea during a composition', () => {
    const { terminal, container } = attach()
    container.dispatch(composition('compositionstart'))
    terminal.textarea.value = 'ni'

    container.dispatch(imeKeyDown())
    const update = insertText('ni', 'insertCompositionText')
    container.dispatch(update)

    expect(terminal.textarea.value).toBe('ni')
    expect(terminal.inputs).toEqual([])
    expect(update.defaultPrevented).toBe(false)
  })

  it('clears only after xterm’s deferred read of the composed text', () => {
    const terminal = new FakeTerminal()
    const container = new FakeEventTarget()
    const textarea = terminal.textarea
    let readValue: string | null = null
    // Model xterm's own listener and its 0ms read: registered while the
    // terminal opened, before the repair attaches.
    textarea.addEventListener('compositionend', () => {
      setTimeout(() => {
        readValue = textarea.value
      }, 0)
    })
    attachSoftKeyboardInput(terminal, container)
    textarea.value = '你好'

    textarea.dispatch(composition('compositionend'))
    // Nothing is cleared while xterm's timer is still pending, not even by a
    // keydown for the following character.
    container.dispatch(imeKeyDown())
    expect(textarea.value).toBe('你好')

    vi.runAllTimers()

    expect(readValue).toBe('你好')
    expect(textarea.value).toBe('')
  })

  it('captures on the container so it runs before xterm’s textarea handlers', () => {
    const { terminal, container } = attach()

    expect(container.registrationsFor('keydown').map((r) => r.capture)).toEqual([true])
    expect(container.registrationsFor('beforeinput').map((r) => r.capture)).toEqual([true])
    expect(container.registrationsFor('compositionstart').map((r) => r.capture)).toEqual([true])
    // The composition clear must not be captured: only registration order can
    // put it after xterm's own listener on the same element.
    expect(terminal.textarea.registrationsFor('compositionend').map((r) => r.capture)).toEqual([
      false,
    ])
  })

  it('detaches every listener and cancels a pending clear', () => {
    const fixture = attach()
    const { terminal, container } = fixture
    const textarea = terminal.textarea
    textarea.value = '你好'
    textarea.dispatch(composition('compositionend'))

    fixture.detach()
    vi.runAllTimers()
    container.dispatch(imeKeyDown())
    const insertion = insertText('d')
    container.dispatch(insertion)

    expect(textarea.value).toBe('你好')
    expect(insertion.defaultPrevented).toBe(false)
    expect(terminal.inputs).toEqual([])
    expect(container.registrationsFor('keydown')).toEqual([])
    expect(container.registrationsFor('beforeinput')).toEqual([])
    expect(container.registrationsFor('compositionstart')).toEqual([])
    expect(textarea.registrationsFor('compositionend')).toEqual([])
  })

  it('never attaches on a platform without a soft keyboard', () => {
    vi.stubGlobal('navigator', { userAgent: DESKTOP_UA, maxTouchPoints: 0 })
    const { terminal, container, detach } = attach()
    terminal.textarea.value = '""'

    container.dispatch(imeKeyDown())
    const insertion = insertText('d')
    container.dispatch(insertion)

    expect(container.registrationsFor('keydown')).toEqual([])
    expect(terminal.textarea.value).toBe('""')
    expect(insertion.defaultPrevented).toBe(false)
    detach()
  })

  it('never attaches in screen-reader mode', () => {
    const terminal = new FakeTerminal()
    terminal.options.screenReaderMode = true
    const container = new FakeEventTarget()

    attachSoftKeyboardInput(terminal, container)

    expect(container.registrationsFor('keydown')).toEqual([])
  })
})
