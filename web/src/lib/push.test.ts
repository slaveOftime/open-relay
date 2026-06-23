import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { fetchPushPublicKey, upsertPushSubscription } from '@/api/client'
import { syncPushSubscription } from './push'

vi.mock('@/api/client', () => ({
  deletePushSubscription: vi.fn(),
  fetchPushPublicKey: vi.fn(),
  upsertPushSubscription: vi.fn(),
}))

const mockedFetchPushPublicKey = vi.mocked(fetchPushPublicKey)
const mockedUpsertPushSubscription = vi.mocked(upsertPushSubscription)

const originalNotification = Object.getOwnPropertyDescriptor(globalThis, 'Notification')
const originalNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator')
const originalWindow = Object.getOwnPropertyDescriptor(globalThis, 'window')

function restoreGlobal(
  name: 'Notification' | 'navigator' | 'window',
  descriptor?: PropertyDescriptor
) {
  if (descriptor) {
    Object.defineProperty(globalThis, name, descriptor)
    return
  }
  Reflect.deleteProperty(globalThis, name)
}

describe('push setup', () => {
  beforeEach(() => {
    vi.resetAllMocks()
  })

  afterEach(() => {
    restoreGlobal('Notification', originalNotification)
    restoreGlobal('navigator', originalNavigator)
    restoreGlobal('window', originalWindow)
  })

  it('requests notification permission before async setup work on explicit enable', async () => {
    const events: string[] = []
    mockedFetchPushPublicKey.mockImplementation(async () => {
      events.push('fetch-key')
      return { public_key: 'AQIDBA' }
    })
    mockedUpsertPushSubscription.mockResolvedValue({ ok: true })

    const localStorage = {
      getItem: vi.fn(() => null),
      setItem: vi.fn(),
      removeItem: vi.fn(),
    }

    const notification = {
      permission: 'default' as NotificationPermission,
      requestPermission: vi.fn(async () => {
        events.push('request-permission')
        notification.permission = 'granted'
        return 'granted' as NotificationPermission
      }),
    }

    const subscription = {
      endpoint: 'https://push.test/subscription',
      toJSON: () => ({
        endpoint: 'https://push.test/subscription',
        keys: { auth: 'auth-key', p256dh: 'p256dh-key' },
      }),
    } as unknown as PushSubscription

    const pushManager = {
      getSubscription: vi.fn(async () => null),
      subscribe: vi.fn(async () => {
        events.push('subscribe')
        return subscription
      }),
    }

    const serviceWorker = {
      register: vi.fn(async () => {
        events.push('register-worker')
        return { pushManager } as unknown as ServiceWorkerRegistration
      }),
    }

    Object.defineProperty(globalThis, 'Notification', {
      configurable: true,
      value: notification,
    })
    Object.defineProperty(globalThis, 'navigator', {
      configurable: true,
      value: { serviceWorker },
    })
    Object.defineProperty(globalThis, 'window', {
      configurable: true,
      value: {
        Notification: notification,
        PushManager: function PushManager() {},
        atob: (value: string) => globalThis.atob(value),
        localStorage,
      },
    })

    const activeSync = syncPushSubscription(true)
    await expect(activeSync).resolves.toBe('subscribed')

    expect(events).toEqual(['request-permission', 'fetch-key', 'register-worker', 'subscribe'])
    expect(notification.requestPermission).toHaveBeenCalledTimes(1)
    expect(serviceWorker.register).toHaveBeenCalledWith('/sw-push.js', { scope: '/__push__/' })
    expect(mockedUpsertPushSubscription).toHaveBeenCalledWith({
      endpoint: 'https://push.test/subscription',
      keys: { auth: 'auth-key', p256dh: 'p256dh-key' },
    })
  })
})
