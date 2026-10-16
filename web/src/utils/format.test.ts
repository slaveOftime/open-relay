import { describe, expect, it } from 'vitest'
import { formatAge, parseArgString, sessionDisplayName } from './format'

describe('parseArgString', () => {
  it('preserves backslashes in quoted Windows path arguments', () => {
    expect(
      parseArgString(
        String.raw`listen --hook "node C:\Users\woo\Documents\Code\jarvis\memory-jarvis\scripts\wechat-bridge-hook.ts --wechat"`
      )
    ).toEqual([
      'listen',
      '--hook',
      'node C:\\Users\\woo\\Documents\\Code\\jarvis\\memory-jarvis\\scripts\\wechat-bridge-hook.ts --wechat',
    ])
  })

  it('preserves backslashes in unquoted Windows path arguments', () => {
    expect(parseArgString(String.raw`--path C:\Users\woo\Documents\Code\jarvis`)).toEqual([
      '--path',
      'C:\\Users\\woo\\Documents\\Code\\jarvis',
    ])
  })

  it('still supports escaped spaces and quotes', () => {
    expect(parseArgString(String.raw`hello\ world "say \"hi\""`)).toEqual([
      'hello world',
      'say "hi"',
    ])
  })
})

describe('formatAge', () => {
  const NOW = Date.parse('2026-10-16T12:00:00.000Z')
  const createdBefore = (seconds: number) => new Date(NOW - seconds * 1000).toISOString()

  it('counts seconds, then minutes, hours and days', () => {
    expect(formatAge(createdBefore(0), NOW)).toBe('0s ago')
    expect(formatAge(createdBefore(59), NOW)).toBe('59s ago')
    expect(formatAge(createdBefore(60), NOW)).toBe('1m ago')
    expect(formatAge(createdBefore(3599), NOW)).toBe('59m ago')
    expect(formatAge(createdBefore(3600), NOW)).toBe('1h ago')
    expect(formatAge(createdBefore(86_399), NOW)).toBe('23h ago')
    expect(formatAge(createdBefore(86_400), NOW)).toBe('1d ago')
    expect(formatAge(createdBefore(3 * 86_400), NOW)).toBe('3d ago')
  })

  it('never counts backwards when a node clock runs ahead of the browser', () => {
    expect(formatAge(createdBefore(-5), NOW)).toBe('0s ago')
  })
})

describe('sessionDisplayName', () => {
  it('preserves backslashes when quoting a Windows hook command for display', () => {
    expect(
      sessionDisplayName({
        command: 'wechat-relay',
        args: [
          'listen',
          '--hook',
          String.raw`node C:\Users\woo\Documents\Code\jarvis\scripts\wechat-bridge-hook.ts --wechat {payload}`,
        ],
      })
    ).toBe(
      String.raw`wechat-relay listen --hook "node C:\Users\woo\Documents\Code\jarvis\scripts\wechat-bridge-hook.ts --wechat {payload}"`
    )
  })
})
