function notificationNavigationUrl(payload) {
  const launch =
    typeof payload?.launch_url === 'string' && payload.launch_url.trim()
      ? payload.launch_url.trim()
      : ''
  if (launch) return new URL(launch, self.location.origin).toString()

  const base =
    typeof payload?.navigation_url === 'string' && payload.navigation_url.trim()
      ? payload.navigation_url.trim()
      : '/'
  const node = typeof payload?.node === 'string' && payload.node.trim() ? payload.node.trim() : ''
  const path =
    !node || !base.startsWith('/')
      ? base
      : `${base}${base.includes('?') ? '&' : '?'}node=${encodeURIComponent(node)}`
  return new URL(path, self.location.origin).toString()
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
    body: description && body ? `${description}\n\n${body}` : description || body,
    tag: payload.session_ids?.[0] || 'open-relay-session-notification',
    data: payload,
  }

  event.waitUntil(self.registration.showNotification(title, options))
})

self.addEventListener('notificationclick', (event) => {
  event.notification.close()

  const targetUrl = notificationNavigationUrl(event.notification?.data || {})

  event.waitUntil(
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then(async (clients) => {
      for (const client of clients) {
        if (!('navigate' in client)) continue
        try {
          const navigated = await client.navigate(targetUrl)
          if (navigated?.focus) return navigated.focus()
        } catch {
          // Fall back to opening a new app window if this client refuses navigation.
        }
      }
      return self.clients.openWindow(targetUrl)
    })
  )
})
