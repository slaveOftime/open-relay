import { describe, expect, it } from 'vitest'
import { sessionActivityKey } from './session-activity'

describe('sessionActivityKey', () => {
  it('keeps identical session ids on different nodes separate', () => {
    expect(sessionActivityKey('same-id', 'worker-a')).not.toBe(
      sessionActivityKey('same-id', 'worker-b')
    )
    expect(sessionActivityKey('same-id', null)).toBe(sessionActivityKey('same-id'))
    expect(sessionActivityKey('same-id', ' worker-a ')).toBe(
      sessionActivityKey('same-id', 'worker-a')
    )
  })
})
