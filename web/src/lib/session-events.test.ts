import { afterEach, describe, expect, it, vi } from 'vitest'

class MockEventSource extends EventTarget {
  static instances: MockEventSource[] = []
  readonly url: string
  onopen: (() => void) | null = null
  onerror: (() => void) | null = null
  close = vi.fn()

  constructor(url: string) {
    super()
    this.url = url
    MockEventSource.instances.push(this)
  }
}

afterEach(() => {
  vi.clearAllTimers()
  vi.useRealTimers()
  vi.unstubAllGlobals()
  vi.resetModules()
  MockEventSource.instances = []
  delete globalThis.__olySparklineStore
})

describe('shared session event stream', () => {
  it('opens one SSE request and delivers activity to the sparkline store', async () => {
    vi.useFakeTimers()
    const browser = Object.assign(new EventTarget(), {
      requestAnimationFrame: (callback: FrameRequestCallback) => setTimeout(() => callback(0), 16),
      cancelAnimationFrame: clearTimeout,
      setInterval,
    })
    vi.stubGlobal('window', browser)
    vi.stubGlobal('navigator', { onLine: true })
    vi.stubGlobal('localStorage', { getItem: () => null })
    vi.stubGlobal('EventSource', MockEventSource)
    const { startSessionEvents, stopSessionEvents, subscribeSessionEvents } =
      await import('./session-events')
    const { recordSessionActivity, getSessionActivitySnapshot } = await import('./session-activity')
    recordSessionActivity({ id: 'session-1', last_total_bytes: 100 })
    const listener = vi.fn()

    // Include the bootstrap's retain plus the page's subscription.
    startSessionEvents()
    const unsubscribe = subscribeSessionEvents(listener)
    await vi.advanceTimersByTimeAsync(16)
    expect(MockEventSource.instances).toHaveLength(1)
    const source = MockEventSource.instances[0]
    expect(source.url).toBe('/api/sessions/events')
    source.dispatchEvent(
      new MessageEvent('session_activity', {
        data: JSON.stringify({
          node: null,
          samples: [{ id: 'session-1', last_total_bytes: 180, last_output_at: null }],
        }),
      })
    )
    expect(listener).toHaveBeenCalledTimes(1)
    expect(getSessionActivitySnapshot('session-1').series.at(-1)).toBe(80)

    unsubscribe()
    expect(source.close).not.toHaveBeenCalled()
    stopSessionEvents()
    expect(source.close).toHaveBeenCalledTimes(1)
  })
})
