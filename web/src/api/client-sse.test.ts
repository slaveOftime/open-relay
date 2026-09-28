import { afterEach, describe, expect, it, vi } from 'vitest'
import { subscribeEvents, type SseConnectionState } from './client'

class MockEventSource extends EventTarget {
  static readonly OPEN = 1
  static readonly instances: MockEventSource[] = []
  readonly url: string
  readyState = 0
  onopen: ((event: Event) => void) | null = null
  onerror: ((event: Event) => void) | null = null

  constructor(url: string) {
    super()
    this.url = url
    MockEventSource.instances.push(this)
  }

  open() {
    this.readyState = MockEventSource.OPEN
    this.onopen?.(new Event('open'))
  }

  close() {
    this.readyState = 2
  }
}

afterEach(() => {
  vi.useRealTimers()
  vi.unstubAllGlobals()
  MockEventSource.instances.length = 0
})

describe('SSE connection status', () => {
  it('ignores stale errors and cancels pending retries when an online event reconnects', () => {
    vi.useFakeTimers()
    const browser = new EventTarget()
    vi.stubGlobal('window', browser)
    vi.stubGlobal('navigator', { onLine: true })
    vi.stubGlobal('localStorage', { getItem: () => null })
    vi.stubGlobal('EventSource', MockEventSource)
    const states: SseConnectionState[] = []
    const onEvent = vi.fn()
    const cleanup = subscribeEvents(onEvent, (state) => states.push(state))

    const first = MockEventSource.instances[0]
    expect(first.url).toBe('/api/sessions/events')
    first.open()
    first.onerror?.(new Event('error'))
    expect(states.at(-1)).toBe('reconnecting')

    browser.dispatchEvent(new Event('online'))
    const second = MockEventSource.instances[1]
    second.open()
    expect(states.at(-1)).toBe('live')

    // The previous source can still have callbacks queued after it was closed.
    first.onerror?.(new Event('error'))
    first.onopen?.(new Event('open'))
    first.dispatchEvent(new MessageEvent('snapshot', { data: '[]' }))
    expect(onEvent).not.toHaveBeenCalled()
    vi.advanceTimersByTime(2_000)
    expect(MockEventSource.instances).toHaveLength(2)
    expect(second.readyState).toBe(MockEventSource.OPEN)
    expect(states.at(-1)).toBe('live')
    browser.dispatchEvent(new Event('online'))
    expect(MockEventSource.instances).toHaveLength(2)
    cleanup()
  })
})
