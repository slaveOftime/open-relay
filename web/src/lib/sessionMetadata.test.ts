import { describe, expect, it } from 'vitest'

import { buildSessionMetadataUpdateSpec, normalizeSessionTitleInput } from './sessionMetadata'

describe('session metadata helpers', () => {
  it('omits unchanged metadata fields', () => {
    expect(
      buildSessionMetadataUpdateSpec(
        { title: 'Deploy', tags: ['prod', 'release'] },
        { title: ' Deploy ', tags: 'prod, release' }
      )
    ).toEqual({})
  })

  it('emits explicit clears for blank title and tags', () => {
    expect(
      buildSessionMetadataUpdateSpec(
        { title: 'Deploy', tags: ['prod'] },
        { title: '   ', tags: '   ' }
      )
    ).toEqual({ title: '', tags: [] })
  })

  it('only includes fields that changed', () => {
    expect(
      buildSessionMetadataUpdateSpec(
        { title: 'Deploy', tags: ['prod'] },
        { title: 'Ship it', tags: 'prod' }
      )
    ).toEqual({ title: 'Ship it' })
  })

  it('normalizes blank title input to null', () => {
    expect(normalizeSessionTitleInput('   ')).toBeNull()
  })
})
