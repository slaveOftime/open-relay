const NOTIFICATION_TARGET_PARAM = 'open-relay-target'
const NOTIFICATION_TARGET_CACHE = 'open-relay-notification-target-v1'
const NOTIFICATION_TARGET_CACHE_KEY = '/__push__/pending-notification-target'

function notificationNavigationPath(payload) {
  const base =
    typeof payload?.navigation_url === 'string' && payload.navigation_url.trim()
      ? payload.navigation_url.trim()
      : '/'
  const node = typeof payload?.node === 'string' && payload.node.trim() ? payload.node.trim() : ''
  return !node || !base.startsWith('/')
    ? base
    : `${base}${base.includes('?') ? '&' : '?'}node=${encodeURIComponent(node)}`
}

function normalizedNotificationTarget(payload) {
  const url = new URL(notificationNavigationPath(payload), self.location.origin)
  if (url.origin !== self.location.origin) return null
  return `${url.pathname}${url.search}${url.hash}`
}

function notificationNavigationUrl(payload) {
  return new URL(notificationNavigationPath(payload), self.location.origin).toString()
}

function usesInAppLaunch(payload) {
  const target = normalizedNotificationTarget(payload)
  if (!target) return false
  const url = new URL(target, self.location.origin)
  return url.pathname === '/' || url.pathname.startsWith('/session/')
}

function notificationLaunchUrl(payload) {
  const launch =
    typeof payload?.launch_url === 'string' && payload.launch_url.trim()
      ? payload.launch_url.trim()
      : ''
  if (launch) return new URL(launch, self.location.origin).toString()
  const target = notificationNavigationPath(payload)
  const normalizedTarget = normalizedNotificationTarget(payload)
  if (!normalizedTarget || !usesInAppLaunch(payload)) {
    return new URL(target, self.location.origin).toString()
  }
  if (normalizedTarget === '/') return new URL('/', self.location.origin).toString()
  const launchUrl = new URL('/', self.location.origin)
  launchUrl.searchParams.set(NOTIFICATION_TARGET_PARAM, normalizedTarget)
  return launchUrl.toString()
}

function notificationMessage(payload) {
  if (!usesInAppLaunch(payload)) return null
  return {
    type: 'open-relay:notification-click',
    target: notificationNavigationUrl(payload),
  }
}

async function storePendingNotificationTarget(payload) {
  const target = normalizedNotificationTarget(payload)
  if (!target) return

  const cache = await self.caches.open(NOTIFICATION_TARGET_CACHE)
  await cache.put(
    NOTIFICATION_TARGET_CACHE_KEY,
    new Response(JSON.stringify({ target }), {
      headers: { 'content-type': 'application/json' },
    })
  )
}

function clientPriority(client) {
  let priority = 0
  if (client.visibilityState === 'visible') priority += 1
  if (client.focused) priority += 2
  return priority
}

self.addEventListener('push', (event) => {
  let payload = {
    title: 'Open Relay notification',
    description: '',
    body: '',
    navigation_url: '/',
    session_ids: [],
  }

  if (event.data) {
    try {
      payload = { ...payload, ...event.data.json() }
    } catch {
      payload = { ...payload, body: event.data.text() }
    }
  }

  const description = (payload.description || '').trim()
  const body = (payload.body || '').trim()
  const title = (payload.title || '').trim() || 'Open Relay notification'
  const options = {
    body: description && body ? `${description}\n${body}` : description || body,
    tag: payload.session_ids?.[0] || 'open-relay-session-notification',
    data: payload,
  }

  event.waitUntil(self.registration.showNotification(title, options))
})

self.addEventListener('notificationclick', (event) => {
  event.notification.close()

  const payload = event.notification?.data || {}
  const navigationUrl = notificationNavigationUrl(payload)
  const launchUrl = notificationLaunchUrl(payload)
  const clickMessage = notificationMessage(payload)

  event.waitUntil(
    Promise.resolve()
      .then(() => storePendingNotificationTarget(payload))
      .then(() => self.clients.matchAll({ type: 'window', includeUncontrolled: true }))
      .then(async (clients) => {
        const sortedClients = [...clients].sort(
          (left, right) => clientPriority(right) - clientPriority(left)
        )

        for (const client of sortedClients) {
          if ('navigate' in client) {
            try {
              const navigated = await client.navigate(navigationUrl)
              const activeClient = navigated || client
              if (activeClient.focus) {
                await activeClient.focus()
              } else if (client.focus) {
                await client.focus()
              }
              if (clickMessage) {
                try {
                  activeClient.postMessage(clickMessage)
                } catch {
                  // A successful navigation is still enough; the app can restore from launchUrl.
                }
              }
              return
            } catch {
              // Fall back to messaging if this client refuses a full navigation.
            }
          }

          try {
            if (clickMessage) {
              if (client.focus) {
                await client.focus()
              }
              client.postMessage(clickMessage)
              return
            }
          } catch {
            // Fall back to opening a new app window if this client refuses messaging too.
          }
        }
        return self.clients.openWindow(launchUrl)
      })
  )
})
