import { describe, expect, it } from 'vitest'

import { buildSessionMetadataUpdateSpec, normalizeSessionTitleInput } from './session-metadata'

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

  it('omits unchanged notifications enabled state', () => {
    expect(
      buildSessionMetadataUpdateSpec(
        { title: 'Deploy', tags: ['prod'], notificationsEnabled: true },
        { title: 'Deploy', tags: 'prod', notificationsEnabled: true }
      )
    ).toEqual({})
  })

  it('includes notifications enabled when toggled', () => {
    expect(
      buildSessionMetadataUpdateSpec(
        { title: 'Deploy', tags: ['prod'], notificationsEnabled: true },
        { title: 'Deploy', tags: 'prod', notificationsEnabled: false }
      )
    ).toEqual({ notifications_enabled: false })
  })

  it('normalizes blank title input to null', () => {
    expect(normalizeSessionTitleInput('   ')).toBeNull()
  })
})
