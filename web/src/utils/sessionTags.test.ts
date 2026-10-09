import { describe, expect, it } from 'vitest'

import { normalizeSessionTags } from './sessionTags'

describe('normalizeSessionTags', () => {
  it('trims whitespace and drops empty entries', () => {
    expect(normalizeSessionTags(['  build ', '', '   '])).toEqual(['build'])
  })

  it('de-duplicates while keeping the first-seen order', () => {
    expect(normalizeSessionTags(['b', 'a', 'b', 'a'])).toEqual(['b', 'a'])
  })

  it('treats trimmed spellings as the same tag', () => {
    expect(normalizeSessionTags(['build', ' build '])).toEqual(['build'])
  })

  it('passes through an already clean list', () => {
    expect(normalizeSessionTags(['ci', 'prod'])).toEqual(['ci', 'prod'])
  })

  it('returns an empty list for anything non-array or empty', () => {
    expect(normalizeSessionTags([])).toEqual([])
    expect(normalizeSessionTags(undefined as unknown as string[])).toEqual([])
    expect(normalizeSessionTags(null as unknown as string[])).toEqual([])
  })
})
