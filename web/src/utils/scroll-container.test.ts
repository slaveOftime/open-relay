import { afterEach, describe, expect, it, vi } from 'vitest'

import { findScrollContainer } from './scroll-container'

/**
 * Stand-in for the two things the function reads: computed `overflow-y` and
 * the element's scroll metrics. The real `HTMLElement` global is replaced by
 * this class so `document.scrollingElement instanceof HTMLElement` has
 * something to test against under the node test environment.
 */
class FakeElement {
  parentElement: FakeElement | null = null
  scrollHeight = 0
  clientHeight = 0
  overflowY = 'visible'
  overflowX = 'visible'

  constructor(
    vertical: string,
    metrics?: { scrollHeight: number; clientHeight: number },
    horizontal = 'visible'
  ) {
    this.overflowY = vertical
    this.overflowX = horizontal
    if (metrics) {
      this.scrollHeight = metrics.scrollHeight
      this.clientHeight = metrics.clientHeight
    }
  }
}

function scrollable(overflowY: string, scrollHeight: number, clientHeight: number): FakeElement {
  return new FakeElement(overflowY, { scrollHeight, clientHeight })
}

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('findScrollContainer', () => {
  it('returns the nearest ancestor that reports auto/scroll overflow and has content to scroll', () => {
    const outer = scrollable('auto', 2000, 500)
    const inner = scrollable('auto', 900, 300)
    const leaf = new FakeElement('visible')
    leaf.parentElement = inner
    inner.parentElement = outer
    vi.stubGlobal('HTMLElement', FakeElement)
    vi.stubGlobal('window', {
      getComputedStyle: (element: FakeElement) => ({ overflowY: element.overflowY }),
    })

    // The inner (nearest) scrollable ancestor wins, not the outer one.
    expect(findScrollContainer(leaf as unknown as HTMLElement)).toBe(inner)
  })

  it('skips a scroll-styled ancestor that has nothing to scroll', () => {
    const notScrollable = scrollable('auto', 200, 500)
    const scrollableAncestor = scrollable('scroll', 4000, 400)
    const leaf = new FakeElement('hidden')
    leaf.parentElement = notScrollable
    notScrollable.parentElement = scrollableAncestor
    vi.stubGlobal('HTMLElement', FakeElement)
    vi.stubGlobal('window', {
      getComputedStyle: (element: FakeElement) => ({ overflowY: element.overflowY }),
    })

    expect(findScrollContainer(leaf as unknown as HTMLElement)).toBe(scrollableAncestor)
  })

  it('ignores containers that only clip, or only scroll horizontally', () => {
    // `overflow-x: auto` with nothing to scroll vertically must not match.
    const clips = scrollable('hidden', 4000, 100)
    const horizontal = new FakeElement('visible', { scrollHeight: 4000, clientHeight: 100 }, 'auto')
    const leaf = new FakeElement('visible')
    leaf.parentElement = horizontal
    horizontal.parentElement = clips
    const documentElement = new FakeElement('visible')
    vi.stubGlobal('HTMLElement', FakeElement)
    vi.stubGlobal('window', {
      getComputedStyle: (element: FakeElement) => ({ overflowY: element.overflowY }),
    })
    vi.stubGlobal('document', { scrollingElement: null, documentElement })

    expect(findScrollContainer(leaf as unknown as HTMLElement)).toBe(documentElement)
  })

  it('prefers the document scrolling element when no ancestor scrolls', () => {
    const plain = new FakeElement('visible')
    const leaf = new FakeElement('visible')
    leaf.parentElement = plain
    const scrollingElement = scrollable('auto', 900, 100)
    vi.stubGlobal('HTMLElement', FakeElement)
    vi.stubGlobal('window', {
      getComputedStyle: (element: FakeElement) => ({ overflowY: element.overflowY }),
    })
    vi.stubGlobal('document', { scrollingElement, documentElement: new FakeElement('visible') })

    expect(findScrollContainer(leaf as unknown as HTMLElement)).toBe(scrollingElement)
  })

  it('walks past a null node to the document root', () => {
    const documentElement = new FakeElement('visible')
    vi.stubGlobal('HTMLElement', FakeElement)
    vi.stubGlobal('window', {
      getComputedStyle: (element: FakeElement) => ({ overflowY: element.overflowY }),
    })
    vi.stubGlobal('document', { scrollingElement: null, documentElement })

    expect(findScrollContainer(null)).toBe(documentElement)
  })
})
