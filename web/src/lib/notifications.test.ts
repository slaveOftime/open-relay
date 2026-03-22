import { describe, expect, it } from 'vitest'

import {
  notificationBody,
  notificationNavigationUrl,
  notificationTag,
  notificationTitle,
} from './notifications'

describe('notifications helpers', () => {
  it('combines description and body for display', () => {
    expect(
      notificationBody({
        description: 'Deploy prod matched a prompt and is waiting for input.',
        body: 'Password:\nEnter MFA code:',
      })
    ).toBe('Deploy prod matched a prompt and is waiting for input.\nPassword:\nEnter MFA code:')
  })

  it('prefers explicit navigation urls', () => {
    expect(notificationNavigationUrl({ navigation_url: '/session/session-123' })).toBe(
      '/session/session-123'
    )
    expect(
      notificationNavigationUrl({
        navigation_url: '/session/session-123?mode=attach',
        node: 'worker 1',
      })
    ).toBe('/session/session-123?mode=attach&node=worker%201')
    expect(notificationNavigationUrl({ navigation_url: '' })).toBe('/')
  })

  it('can resolve absolute same-origin navigation urls for service-worker clicks', () => {
    expect(
      notificationNavigationUrl(
        {
          navigation_url: '/session/session-123?mode=attach',
          node: 'worker 1',
        },
        'https://relay.test'
      )
    ).toBe('https://relay.test/session/session-123?mode=attach&node=worker%201')
  })

  it('uses sensible title and tag fallbacks', () => {
    expect(notificationTitle({ title: 'Input required' })).toBe('Input required')
    expect(notificationTitle({ title: '   ' })).toBe('Open Relay notification')
    expect(notificationTag({ session_ids: ['session-123'] })).toBe('session-123')
    expect(notificationTag({ session_ids: [] })).toBe('open-relay-session-notification')
  })
})
