import { describe, expect, it } from 'vitest'
import type { NodeSummary } from '@/api/types'
import { nodeAfterSwipe } from './sessions-node-swipe'

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
