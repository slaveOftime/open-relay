import { describe, expect, it } from 'vitest'

import {
  RECONNECT_BASE_DELAY_MS,
  RECONNECT_MAX_DELAY_MS,
  deferredReconnectReason,
  isTransientClose,
  reconnectDelayMs,
  shouldDeferReconnect,
} from './attach-reconnect-policy'

describe('reconnectDelayMs', () => {
  it('doubles from the base delay for the first few attempts', () => {
    expect(reconnectDelayMs(1)).toBe(RECONNECT_BASE_DELAY_MS)
    expect(reconnectDelayMs(2)).toBe(240)
    expect(reconnectDelayMs(3)).toBe(480)
    expect(reconnectDelayMs(4)).toBe(960)
  })

  it('clamps to the cap rather than growing without bound', () => {
    expect(reconnectDelayMs(5)).toBe(1920)
    expect(reconnectDelayMs(6)).toBe(RECONNECT_MAX_DELAY_MS)
    // The exponent is clamped at 6, so 7 and 7000 compute the same delay.
    expect(reconnectDelayMs(7)).toBe(RECONNECT_MAX_DELAY_MS)
    expect(reconnectDelayMs(7000)).toBe(RECONNECT_MAX_DELAY_MS)
  })

  it('never exceeds the cap at any attempt in the clamp window', () => {
    for (let attempt = 1; attempt <= 500; attempt += 1) {
      expect(reconnectDelayMs(attempt)).toBeLessThanOrEqual(RECONNECT_MAX_DELAY_MS)
      expect(reconnectDelayMs(attempt)).toBeGreaterThan(0)
    }
  })

  it('treats an attempt below one as nothing to wait for', () => {
    expect(reconnectDelayMs(0)).toBe(0)
    expect(reconnectDelayMs(-1)).toBe(0)
  })
})

describe('isTransientClose', () => {
  it('accepts the codes a killed connection produces', () => {
    // 1006 abnormal, 1001 going away, 1005 no status, 0 before the handshake.
    for (const code of [1006, 1001, 1005, 0]) {
      expect(isTransientClose(code)).toBe(true)
    }
  })

  it('rejects the codes that mean the session is gone', () => {
    for (const code of [1000, 1002, 1008, 1011, 4000, 4404]) {
      expect(isTransientClose(code)).toBe(false)
    }
  })
})

describe('shouldDeferReconnect', () => {
  it('defers while hidden or offline', () => {
    expect(shouldDeferReconnect({ hidden: true, offline: false })).toBe(true)
    expect(shouldDeferReconnect({ hidden: false, offline: true })).toBe(true)
    expect(shouldDeferReconnect({ hidden: true, offline: true })).toBe(true)
  })

  it('does not defer while visible and online', () => {
    expect(shouldDeferReconnect({ hidden: false, offline: false })).toBe(false)
  })
})

describe('deferredReconnectReason', () => {
  it('joins the active reasons with a plus', () => {
    expect(deferredReconnectReason({ hidden: true, offline: false })).toBe('hidden')
    expect(deferredReconnectReason({ hidden: false, offline: true })).toBe('offline')
    expect(deferredReconnectReason({ hidden: true, offline: true })).toBe('hidden+offline')
    expect(deferredReconnectReason({ hidden: false, offline: false })).toBe('')
  })
})
