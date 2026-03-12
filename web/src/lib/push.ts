import { deletePushSubscription, fetchPushPublicKey, upsertPushSubscription } from '@/api/client'
import type { PushSubscriptionInput } from '@/api/types'

export type PushSetupState = 'unsupported' | 'unconfigured' | 'denied' | 'idle' | 'subscribed'

const PUSH_SW_SCOPE = '/__push__/'
const PUSH_DISABLED_KEY = 'open-relay.web.push.disabled.v1'

let passiveSyncInFlight: Promise<PushSetupState> | null = null
let passiveSyncCachedState: PushSetupState | null = null

function isPushExplicitlyDisabled(): boolean {
  if (typeof window === 'undefined') return false
  try {
    return window.localStorage.getItem(PUSH_DISABLED_KEY) === 'true'
  } catch {
    return false
  }
}

function setPushExplicitlyDisabled(disabled: boolean) {
  if (typeof window === 'undefined') return
  try {
    if (disabled) {
      window.localStorage.setItem(PUSH_DISABLED_KEY, 'true')
      return
    }
    window.localStorage.removeItem(PUSH_DISABLED_KEY)
  } catch {
    // Ignore storage write failures; push state still follows browser capabilities.
  }
}

function urlBase64ToArrayBuffer(base64String: string): ArrayBuffer {
  const padding = '='.repeat((4 - (base64String.length % 4)) % 4)
  const base64 = (base64String + padding).replace(/-/g, '+').replace(/_/g, '/')
  const rawData = window.atob(base64)
  const bytes = Uint8Array.from(rawData, (char) => char.charCodeAt(0))
  return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength)
}

async function registerPushWorker(): Promise<ServiceWorkerRegistration | null> {
  if (!('serviceWorker' in navigator)) return null
  // Keep push SW on an isolated scope so it never competes with the app PWA SW
  // at "/" (which can trigger controller churn and launch flashing on iOS).
  return navigator.serviceWorker.register('/sw-push.js', { scope: PUSH_SW_SCOPE })
}

function toPushSubscriptionInput(sub: PushSubscription): PushSubscriptionInput | null {
  const json = sub.toJSON()
  const p256dh = json.keys?.p256dh
  const auth = json.keys?.auth
  const endpoint = json.endpoint
  if (!endpoint || !p256dh || !auth) return null
  return {
    endpoint,
    keys: { p256dh, auth },
  }
}

export async function syncPushSubscription(requestPermission: boolean): Promise<PushSetupState> {
  const explicitlyDisabled = isPushExplicitlyDisabled()
  if (!requestPermission && passiveSyncCachedState && !explicitlyDisabled) {
    return passiveSyncCachedState
  }
  if (passiveSyncInFlight) {
    if (!requestPermission) return passiveSyncInFlight
    await passiveSyncInFlight.catch(() => {})
  }

  const run = (async (): Promise<PushSetupState> => {
    if (
      !('serviceWorker' in navigator) ||
      !('PushManager' in window) ||
      !('Notification' in window)
    ) {
      return 'unsupported'
    }

    const res = await fetchPushPublicKey()
    if (!res.public_key) return 'unconfigured'
    const publicKey = res.public_key

    const registration = await registerPushWorker()
    if (!registration) return 'unsupported'

    let permission = Notification.permission
    if (permission === 'default' && requestPermission) {
      permission = await Notification.requestPermission()
    }

    if (permission === 'denied') {
      const deniedSub = await registration.pushManager.getSubscription()
      if (deniedSub) {
        await deletePushSubscription(deniedSub.endpoint).catch(() => {})
        await deniedSub.unsubscribe().catch(() => {})
      }
      return 'denied'
    }
    if (permission !== 'granted') return 'idle'
    if (!requestPermission && explicitlyDisabled) return 'idle'

    let subscription = await registration.pushManager.getSubscription()
    if (!subscription) {
      subscription = await registration.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: urlBase64ToArrayBuffer(publicKey),
      })
    }

    const payload = toPushSubscriptionInput(subscription)
    if (!payload) return 'idle'

    await upsertPushSubscription(payload)
    setPushExplicitlyDisabled(false)
    return 'subscribed'
  })()

  passiveSyncInFlight = run
  run
    .then((state) => {
      passiveSyncCachedState = state
    })
    .finally(() => {
      passiveSyncInFlight = null
    })
  return run
}

export async function disablePushNotifications(): Promise<PushSetupState> {
  const nextState = Notification.permission === 'denied' ? 'denied' : 'idle'
  if (!('serviceWorker' in navigator)) {
    passiveSyncCachedState = nextState
    setPushExplicitlyDisabled(true)
    return nextState
  }
  const registration = await navigator.serviceWorker.getRegistration(PUSH_SW_SCOPE)
  const subscription = await registration?.pushManager.getSubscription()
  if (!subscription) {
    passiveSyncCachedState = nextState
    setPushExplicitlyDisabled(true)
    return nextState
  }

  await deletePushSubscription(subscription.endpoint)
  await subscription.unsubscribe()
  passiveSyncCachedState = nextState
  setPushExplicitlyDisabled(true)
  return nextState
}

export async function showSessionNotification(
  summary: string,
  body: string,
  tag: string
): Promise<void> {
  if (!('Notification' in window) || Notification.permission !== 'granted') return

  const registration = await navigator.serviceWorker.getRegistration(PUSH_SW_SCOPE)
  if (registration) {
    await registration.showNotification(summary, {
      body,
      tag,
    })
    return
  }

  new Notification(summary, { body, tag })
}
