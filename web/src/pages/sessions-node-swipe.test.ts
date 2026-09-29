import { describe, expect, it } from 'vitest'
import type { NodeSummary } from '@/api/types'
import {
  nodeAfterSwipe,
  pullDragOffset,
  shouldRefreshOnPull,
  swipeDragOffset,
} from './sessions-node-swipe'

const nodes: NodeSummary[] = [
  { name: 'worker-a', connected: true },
  { name: 'worker-b', connected: true },
]

describe('nodeAfterSwipe', () => {
  it('cycles local and connected nodes in both directions', () => {
    expect(nodeAfterSwipe(null, nodes, -100, 10)).toBe('worker-a')
    expect(nodeAfterSwipe('worker-a', nodes, -100, 10)).toBe('worker-b')
    expect(nodeAfterSwipe('worker-b', nodes, -100, 10)).toBeNull()
    expect(nodeAfterSwipe(null, nodes, 100, 10)).toBe('worker-b')
    expect(nodeAfterSwipe('worker-b', nodes, 100, 10)).toBe('worker-a')
  })

  it('ignores short and vertical or diagonal drags', () => {
    expect(nodeAfterSwipe(null, nodes, -50, 0)).toBeUndefined()
    expect(nodeAfterSwipe(null, nodes, -100, 100)).toBeUndefined()
    expect(nodeAfterSwipe(null, nodes, -100, 150)).toBeUndefined()
  })

  it('does nothing without other nodes and recovers unavailable selections', () => {
    expect(nodeAfterSwipe(null, [], -100, 0)).toBeUndefined()
    expect(nodeAfterSwipe('disconnected', [], -100, 0)).toBeNull()
    expect(nodeAfterSwipe('disconnected', nodes, -100, 0)).toBeNull()
    expect(nodeAfterSwipe(null, [...nodes, nodes[0]], -100, 0)).toBe('worker-a')
  })
})

describe('swipeDragOffset', () => {
  it('follows a horizontal drag with resistance and caps travel', () => {
    expect(swipeDragOffset(-100, 5, true)).toBe(-30)
    expect(swipeDragOffset(200, 10, true)).toBe(48)
    expect(swipeDragOffset(-200, 10, true)).toBe(-48)
  })

  it('leaves taps, vertical scrolling and single-node views alone', () => {
    expect(swipeDragOffset(9, 0, true)).toBe(0)
    expect(swipeDragOffset(100, 90, true)).toBe(0)
    expect(swipeDragOffset(100, 0, false)).toBe(0)
  })
})

describe('pull-to-refresh gesture', () => {
  it('shows resisted feedback from the top before committing refresh', () => {
    expect(pullDragOffset(2, 60, true)).toBe(36)
    expect(pullDragOffset(0, 160, true)).toBe(72)
    expect(shouldRefreshOnPull(0, 87, true)).toBe(false)
    expect(shouldRefreshOnPull(0, 88, true)).toBe(true)
  })

  it('ignores scrolling down the list, upward pulls and horizontal node swipes', () => {
    expect(pullDragOffset(0, 120, false)).toBe(0)
    expect(shouldRefreshOnPull(0, 120, false)).toBe(false)
    expect(pullDragOffset(0, -120, true)).toBe(0)
    expect(pullDragOffset(110, 100, true)).toBe(0)
    expect(shouldRefreshOnPull(110, 100, true)).toBe(false)
  })
})
