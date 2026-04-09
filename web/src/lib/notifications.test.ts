import { describe, expect, it } from 'vitest'

import {
  NOTIFICATION_CLICK_MESSAGE,
  consumePendingNotificationTarget,
  notificationBody,
  notificationClickMessageTarget,
  notificationLaunchTargetFromUrl,
  notificationLaunchUrl,
  normalizeNotificationTarget,
  notificationNavigationUrl,
  notificationTag,
  notificationTitle,
  storePendingNotificationTarget,
} from './notifications'

class TestResponse {
  private readonly body: string

  constructor(body: string) {
    this.body = body
  }

  async json(): Promise<unknown> {
    return JSON.parse(this.body)
  }
}

describe('notifications helpers', () => {
  it('stores and consumes pending notification targets via Cache Storage', async () => {
    const entries = new Map<string, TestResponse>()
    const cache = {
      put: async (key: string, value: TestResponse) => {
        entries.set(key, value)
      },
      match: async (key: string) => entries.get(key) ?? null,
      delete: async (key: string) => entries.delete(key),
    }

    Object.defineProperty(globalThis, 'Response', {
      configurable: true,
      value: TestResponse,
    })
    Object.defineProperty(globalThis, 'caches', {
      configurable: true,
      value: {
        open: async () => cache,
      },
    })

    await storePendingNotificationTarget(
      'https://relay.test/session/session-123?mode=attach',
      'https://relay.test'
    )

    await expect(consumePendingNotificationTarget('https://relay.test')).resolves.toBe(
      '/session/session-123?mode=attach'
    )
    await expect(consumePendingNotificationTarget('https://relay.test')).resolves.toBeNull()
  })

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

  it('wraps deep links in a stable root launch url for standalone notification opens', () => {
    expect(
      notificationLaunchUrl(
        {
          navigation_url: '/session/session-123?mode=attach',
          node: 'worker 1',
        },
        'https://relay.test'
      )
    ).toBe(
      'https://relay.test/?open-relay-target=%2Fsession%2Fsession-123%3Fmode%3Dattach%26node%3Dworker%25201'
    )
    expect(notificationLaunchUrl({ navigation_url: '' }, 'https://relay.test')).toBe(
      'https://relay.test/'
    )
    expect(notificationLaunchUrl({ navigation_url: '/apps/todo' }, 'https://relay.test')).toBe(
      'https://relay.test/apps/todo'
    )
    expect(notificationLaunchUrl({ navigation_url: 'https://bing.com' }, 'https://relay.test')).toBe(
      'https://bing.com/'
    )
    expect(
      notificationLaunchUrl(
        { navigation_url: 'https://relay.test/session/session-123?mode=attach' },
        'https://relay.test'
      )
    ).toBe(
      'https://relay.test/?open-relay-target=%2Fsession%2Fsession-123%3Fmode%3Dattach'
    )
  })

  it('extracts wrapped notification launch targets during app startup', () => {
    expect(
      notificationLaunchTargetFromUrl(
        'https://relay.test/?open-relay-target=%2Fsession%2Fsession-123%3Fmode%3Dattach%26node%3Dworker%25201'
      )
    ).toBe('/session/session-123?mode=attach&node=worker%201')
    expect(notificationLaunchTargetFromUrl('https://relay.test/')).toBeNull()
  })

  it('normalizes notification targets and rejects cross-origin urls', () => {
    expect(
      normalizeNotificationTarget('/session/session-123?mode=attach', 'https://relay.test')
    ).toBe('/session/session-123?mode=attach')
    expect(
      normalizeNotificationTarget(
        'https://relay.test/session/session-123?mode=attach',
        'https://relay.test'
      )
    ).toBe('/session/session-123?mode=attach')
    expect(
      normalizeNotificationTarget(
        'https://evil.test/session/session-123?mode=attach',
        'https://relay.test'
      )
    ).toBeNull()
  })

  it('extracts deep-link targets from service worker click messages', () => {
    expect(
      notificationClickMessageTarget(
        {
          type: NOTIFICATION_CLICK_MESSAGE,
          target: 'https://relay.test/session/session-123?mode=attach&node=worker%201',
        },
        'https://relay.test'
      )
    ).toBe('/session/session-123?mode=attach&node=worker%201')
    expect(
      notificationClickMessageTarget({ type: NOTIFICATION_CLICK_MESSAGE }, 'https://relay.test')
    ).toBeNull()
    expect(
      notificationClickMessageTarget(
        {
          type: NOTIFICATION_CLICK_MESSAGE,
          target: 'https://evil.test/session/session-123?mode=attach',
        },
        'https://relay.test'
      )
    ).toBeNull()
  })

  it('uses sensible title and tag fallbacks', () => {
    expect(notificationTitle({ title: 'Input required' })).toBe('Input required')
    expect(notificationTitle({ title: '   ' })).toBe('Open Relay notification')
    expect(notificationTag({ session_ids: ['session-123'] })).toBe('session-123')
    expect(notificationTag({ session_ids: [] })).toBe('open-relay-session-notification')
  })
})
