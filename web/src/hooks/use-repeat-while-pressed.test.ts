import { describe, expect, it, vi } from 'vitest'
import type {
  ButtonHTMLAttributes,
  FocusEvent as ReactFocusEvent,
  KeyboardEvent as ReactKeyboardEvent,
  MouseEvent as ReactMouseEvent,
  PointerEvent as ReactPointerEvent,
} from 'react'
import { holdRepeatProps, type RepeatControls } from './use-repeat-while-pressed'

/**
 * The handlers are a plain object of DOM callbacks, so the contract can be
 * exercised without a renderer: press behavior, focus parking, and the
 * click/pointer disambiguation that keeps assistive tech working.
 */

function fakeButton() {
  return { focus: vi.fn() }
}

function fakeEvent(overrides: Record<string, unknown> = {}) {
  return {
    button: 0,
    pointerId: 1,
    timeStamp: 1000,
    currentTarget: fakeButton(),
    preventDefault: vi.fn(),
    ...overrides,
  }
}

function pointerEvent(
  overrides: Record<string, unknown> = {}
): ReactPointerEvent<HTMLButtonElement> {
  return fakeEvent(overrides) as unknown as ReactPointerEvent<HTMLButtonElement>
}

function setup(overrides: Partial<RepeatControls> = {}) {
  const controls: RepeatControls = {
    startRepeat: vi.fn(),
    stopRepeat: vi.fn(),
    endsOnPointer: () => true,
    endsOnKey: () => true,
    isFollowUpClick: () => false,
    ...overrides,
  }
  const action = vi.fn()
  const props = holdRepeatProps(controls, action) as Required<
    ButtonHTMLAttributes<HTMLButtonElement>
  >
  const raw = fakeEvent()
  const event = raw as unknown as ReactPointerEvent<HTMLButtonElement>
  return { controls, action, props, event, raw, button: raw.currentTarget }
}

describe('holdRepeatProps press handling', () => {
  it('sends immediately, parks focus on the button and claims the pointer', () => {
    const { props, controls, action, event, button } = setup()

    props.onPointerDown?.(event)

    expect(event.preventDefault).toHaveBeenCalled()
    // The fix for the keyboard popping up: xterm's hidden textarea keeps DOM
    // focus in attach mode unless the pressed control claims it.
    expect(button.focus).toHaveBeenCalledWith({ preventScroll: true })
    expect(controls.startRepeat).toHaveBeenCalledWith(action, {
      pointerId: 1,
      at: 1000,
    })
  })

  it('ignores non-primary mouse buttons', () => {
    const { props, controls, event, button } = setup()
    props.onPointerDown?.(pointerEvent({ button: 2 }))
    expect(controls.startRepeat).not.toHaveBeenCalled()
    expect(event.preventDefault).not.toHaveBeenCalled()
    expect(button.focus).not.toHaveBeenCalled()
  })

  it('parks focus on mousedown too, for browsers that still send it', () => {
    const { props, raw, button } = setup()
    props.onMouseDown?.(fakeEvent(raw) as unknown as ReactMouseEvent<HTMLButtonElement>)
    expect(raw.preventDefault).toHaveBeenCalled()
    expect(button.focus).toHaveBeenCalledWith({ preventScroll: true })
  })
})

describe('holdRepeatProps keyboard handling', () => {
  it('starts on Enter and Space and claims the key', () => {
    const { props, controls, action, raw } = setup()
    const keyboard = (overrides: Record<string, unknown> = {}) =>
      fakeEvent({ ...raw, ...overrides }) as unknown as ReactKeyboardEvent<HTMLButtonElement>

    props.onKeyDown?.(keyboard({ key: 'Enter' }))
    expect(controls.startRepeat).toHaveBeenCalledWith(action, { key: 'Enter', at: 1000 })

    props.onKeyDown?.(keyboard({ key: ' ' }))
    expect(controls.startRepeat).toHaveBeenLastCalledWith(action, { key: ' ', at: 1000 })
  })

  it('ignores other keys and auto-repeat keydowns', () => {
    const { props, controls, raw } = setup()
    const keyboard = (overrides: Record<string, unknown> = {}) =>
      fakeEvent({ ...raw, ...overrides }) as unknown as ReactKeyboardEvent<HTMLButtonElement>

    props.onKeyDown?.(keyboard({ key: 'a' }))
    props.onKeyDown?.(keyboard({ key: 'Enter', repeat: true }))
    expect(controls.startRepeat).not.toHaveBeenCalled()
  })

  it('stops on key up and blur', () => {
    const { props, controls, raw } = setup()
    props.onKeyUp?.(fakeEvent(raw) as unknown as ReactKeyboardEvent<HTMLButtonElement>)
    props.onBlur?.(fakeEvent(raw) as unknown as ReactFocusEvent<HTMLButtonElement>)
    expect(controls.stopRepeat).toHaveBeenCalledTimes(2)
  })
})

describe('holdRepeatProps click handling', () => {
  it('ignores the click that follows the press it already handled', () => {
    const { props, action, controls } = setup({ isFollowUpClick: () => true })
    props.onClick?.(pointerEvent({ timeStamp: 1040 }))
    expect(action).not.toHaveBeenCalled()
    expect(controls.stopRepeat).not.toHaveBeenCalled()
  })

  it('sends once for a click with no preceding press', () => {
    // Assistive technology and element.click() activate buttons this way.
    const { props, action, controls } = setup({ isFollowUpClick: () => false })
    props.onClick?.(pointerEvent({ timeStamp: 0 }))
    expect(action).toHaveBeenCalledTimes(1)
    expect(controls.stopRepeat).toHaveBeenCalled()
  })
})

describe('holdRepeatProps pointer teardown', () => {
  it('stops the hold when the pointer ends, leaves or is cancelled', () => {
    const { props, controls, event } = setup()
    props.onPointerUp?.(event)
    props.onPointerLeave?.(event)
    props.onPointerCancel?.(event)
    expect(controls.stopRepeat).toHaveBeenCalledTimes(3)
  })

  it('suppresses the context menu so long presses do not open one', () => {
    const { props, event } = setup()
    props.onContextMenu?.(event)
    expect(event.preventDefault).toHaveBeenCalled()
  })
})
