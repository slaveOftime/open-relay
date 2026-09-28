import type { NodeSummary } from '@/api/types'

/** Left goes to the next node, right to the previous; undefined means no swipe. */
export function nodeAfterSwipe(
  selectedNode: string | null,
  nodes: readonly NodeSummary[],
  deltaX: number,
  deltaY: number
): string | null | undefined {
  if (Math.abs(deltaX) < 72 || Math.abs(deltaX) <= Math.abs(deltaY) * 1.5) return undefined
  const choices: (string | null)[] = [null, ...new Set(nodes.map((node) => node.name))]
  if (choices.length === 1) return selectedNode ? null : undefined
  const index = choices.indexOf(selectedNode)
  if (index < 0) return null // an unavailable saved node can always return to local
  return choices[(index + (deltaX < 0 ? 1 : -1) + choices.length) % choices.length]
}
