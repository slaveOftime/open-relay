import type { NodeSummary } from '@/api/types'

/**
 * Cross-axis threshold shared by nodeAfterSwipe and swipeDragOffset. We require
 * the horizontal component to be at least as large as the vertical one so a
 * mostly-horizontal drag always wins over a barely-vertical pull, which is how
 * users expect a "swipe to switch node" gesture to feel. Pull still keeps its
 * stricter 1.5× ratio in shouldRefreshOnPull, so a clearly-vertical drag at
 * the scroll top still commits to refresh.
 */
const SWIPE_CROSS_AXIS_RATIO = 1.0

/** Left goes to the next node, right to the previous; undefined means no swipe. */
export function nodeAfterSwipe(
  selectedNode: string | null,
  nodes: readonly NodeSummary[],
  deltaX: number,
  deltaY: number
): string | null | undefined {
  if (
    Math.abs(deltaX) < 72 ||
    Math.abs(deltaX) <= Math.abs(deltaY) * SWIPE_CROSS_AXIS_RATIO
  ) {
    return undefined
  }
  const choices: (string | null)[] = [null, ...new Set(nodes.map((node) => node.name))]
  if (choices.length === 1) return selectedNode ? null : undefined
  const index = choices.indexOf(selectedNode)
  if (index < 0) return null // an unavailable saved node can always return to local
  return choices[(index + (deltaX < 0 ? 1 : -1) + choices.length) % choices.length]
}

/** Small, resisted visual follow-through; does not commit navigation. */
export function swipeDragOffset(deltaX: number, deltaY: number, canSwitch: boolean): number {
  if (
    !canSwitch ||
    Math.abs(deltaX) < 10 ||
    Math.abs(deltaX) <= Math.abs(deltaY) * SWIPE_CROSS_AXIS_RATIO
  ) {
    return 0
  }
  return Math.sign(deltaX) * Math.min(48, Math.abs(deltaX) * 0.3)
}

export const PULL_REFRESH_THRESHOLD = 88

/** Only a downward, mainly vertical drag from the scroll top previews refresh. */
export function pullDragOffset(deltaX: number, deltaY: number, atTop: boolean): number {
  if (!atTop || deltaY < 10 || deltaY <= Math.abs(deltaX) * 1.5) return 0
  return Math.min(72, deltaY * 0.6)
}

export function shouldRefreshOnPull(deltaX: number, deltaY: number, atTop: boolean): boolean {
  return atTop && deltaY >= PULL_REFRESH_THRESHOLD && deltaY > Math.abs(deltaX) * 1.5
}
