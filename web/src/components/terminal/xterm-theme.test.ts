import { afterEach, describe, expect, it, vi } from 'vitest'

import {
  getTerminalTheme,
  subscribeTerminalColorScheme,
  type TerminalColorScheme,
} from './xterm-theme'

/**
 * The suite runs in the `node` environment (see vite.config.ts), so these build
 * the minimum window/document shape the resolver reads and a MutationObserver
 * that lets a test fire its callback.
 */

const DARK_BACKGROUND = '#030712'
const LIGHT_BACKGROUND = '#f1f5f9'

class FakeMutationObserver {
  readonly callback: () => void
  target: unknown = null
  options: MutationObserverInit | undefined
  disconnected = false

  constructor(callback: () => void) {
    this.callback = callback
  }

  observe(target: unknown, options?: MutationObserverInit): void {
    this.target = target
    this.options = options
  }

  disconnect(): void {
    this.disconnected = true
  }

  /** A real observer stops delivering after `disconnect()`. */
  notify(): void {
    if (!this.disconnected) this.callback()
  }
}

interface SchemeStub {
  /** Flip `(prefers-color-scheme: dark)` and fire the media listeners. */
  setSystemDark(dark: boolean): void
  /** Set or remove the `data-theme` attribute and fire the observers. */
  setDataTheme(theme: string | undefined): void
  mediaListeners: Set<() => void>
  observers: FakeMutationObserver[]
}

function stubScheme(initial: { systemDark: boolean; dataTheme?: string }): SchemeStub {
  let systemDark = initial.systemDark
  const mediaListeners = new Set<() => void>()
  const media = {
    get matches() {
      return systemDark
    },
    addEventListener(type: string, listener: () => void) {
      if (type === 'change') mediaListeners.add(listener)
    },
    removeEventListener(type: string, listener: () => void) {
      if (type === 'change') mediaListeners.delete(listener)
    },
  }
  vi.stubGlobal('window', { matchMedia: () => media })

  const dataset: { theme?: string } =
    initial.dataTheme === undefined ? {} : { theme: initial.dataTheme }
  const documentElement = { dataset }
  vi.stubGlobal('document', { documentElement })

  const observers: FakeMutationObserver[] = []
  class RecordingObserver extends FakeMutationObserver {
    constructor(callback: () => void) {
      super(callback)
      observers.push(this)
    }
  }
  vi.stubGlobal('MutationObserver', RecordingObserver)

  return {
    mediaListeners,
    observers,
    setSystemDark(dark) {
      if (dark === systemDark) return
      systemDark = dark
      for (const listener of [...mediaListeners]) listener()
    },
    setDataTheme(theme) {
      if (theme === dataset.theme) return
      if (theme === undefined) delete dataset.theme
      else dataset.theme = theme
      for (const observer of observers) observer.notify()
    },
  }
}

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('getTerminalTheme', () => {
  it('paints the dark palette when the system prefers dark', () => {
    stubScheme({ systemDark: true })

    expect(getTerminalTheme().background).toBe(DARK_BACKGROUND)
  })

  it('paints the light palette when the system prefers light', () => {
    stubScheme({ systemDark: false })

    expect(getTerminalTheme().background).toBe(LIGHT_BACKGROUND)
  })

  it('lets an explicit light data-theme win over a dark system', () => {
    stubScheme({ systemDark: true, dataTheme: 'light' })

    expect(getTerminalTheme().background).toBe(LIGHT_BACKGROUND)
  })

  it('lets an explicit dark data-theme win over a light system', () => {
    stubScheme({ systemDark: false, dataTheme: 'dark' })

    expect(getTerminalTheme().background).toBe(DARK_BACKGROUND)
  })

  it('falls back to the system for a data-theme that is not light or dark', () => {
    stubScheme({ systemDark: true, dataTheme: 'sepia' })

    expect(getTerminalTheme().background).toBe(DARK_BACKGROUND)
  })
})

describe('subscribeTerminalColorScheme', () => {
  it('reports the system preference flipping either way', () => {
    const stub = stubScheme({ systemDark: false })
    const seen: TerminalColorScheme[] = []
    const unsubscribe = subscribeTerminalColorScheme((scheme) => seen.push(scheme))

    stub.setSystemDark(true)
    stub.setSystemDark(false)

    expect(seen).toEqual(['dark', 'light'])
    unsubscribe()
  })

  it('reports an explicit theme being set and removed', () => {
    const stub = stubScheme({ systemDark: true })
    const seen: TerminalColorScheme[] = []
    subscribeTerminalColorScheme((scheme) => seen.push(scheme))

    stub.setDataTheme('light')
    stub.setDataTheme(undefined)

    expect(seen).toEqual(['light', 'dark'])
  })

  it('stays quiet while the resolved scheme is unchanged', () => {
    const stub = stubScheme({ systemDark: true })
    const seen: TerminalColorScheme[] = []
    subscribeTerminalColorScheme((scheme) => seen.push(scheme))

    // A mutation that changes the attribute but agrees with the OS preference.
    stub.setDataTheme('dark')

    expect(seen).toEqual([])
  })

  it('watches only the data-theme attribute on the document element', () => {
    const stub = stubScheme({ systemDark: false })
    subscribeTerminalColorScheme(() => {})

    expect(stub.observers).toHaveLength(1)
    expect(stub.observers[0].options).toEqual({
      attributes: true,
      attributeFilter: ['data-theme'],
    })
    expect(stub.observers[0].target).toBe(document.documentElement)
  })

  it('stops reporting after unsubscribe', () => {
    const stub = stubScheme({ systemDark: false })
    const seen: TerminalColorScheme[] = []
    const unsubscribe = subscribeTerminalColorScheme((scheme) => seen.push(scheme))

    unsubscribe()
    stub.setSystemDark(true)
    stub.setDataTheme('dark')

    expect(seen).toEqual([])
    expect(stub.mediaListeners.size).toBe(0)
    expect(stub.observers[0].disconnected).toBe(true)
  })
})
