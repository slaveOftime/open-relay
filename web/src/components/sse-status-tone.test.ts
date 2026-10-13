import { describe, expect, it } from 'vitest'
import { sseStatusTone } from './sse-status-tone'

describe('sseStatusTone', () => {
  it('keeps the first handshake quiet', () => {
    const tone = sseStatusTone('connecting')
    expect(tone.label).toBe('Connecting…')
    expect(tone.pulse).toBe(false)
    // Never the error colour while nothing has gone wrong yet.
    expect(tone.dot).not.toContain('red')
  })

  it('only pulses degraded connections', () => {
    expect(sseStatusTone('live').pulse).toBe(false)
    expect(sseStatusTone('live').label).toBe('Live')
    expect(sseStatusTone('reconnecting').pulse).toBe(true)
    expect(sseStatusTone('offline').pulse).toBe(true)
  })

  it('falls back to the quiet state for unknown values', () => {
    expect(sseStatusTone('nope' as never).label).toBe('Connecting…')
  })
})
