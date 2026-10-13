/**
 * Walk up from a node to the nearest element that can actually scroll
 * vertically, falling back to the document's scrolling element.
 *
 * Used by the terminal and the attach panel to decide what to scroll when new
 * output arrives or the drawer opens, so the rule lives in one place instead of
 * two near-copies that can drift apart.
 */

export function findScrollContainer(node: HTMLElement | null): HTMLElement | null {
  let current = node?.parentElement ?? null
  while (current) {
    const style = window.getComputedStyle(current)
    const overflowY = style.overflowY
    const canScroll =
      (overflowY === 'auto' || overflowY === 'scroll') &&
      current.scrollHeight > current.clientHeight
    if (canScroll) {
      return current
    }
    current = current.parentElement
  }

  return document.scrollingElement instanceof HTMLElement
    ? document.scrollingElement
    : document.documentElement
}
