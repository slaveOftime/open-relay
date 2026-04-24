import { describe, expect, it } from 'vitest'
import { parseArgString, sessionDisplayName } from './format'

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
