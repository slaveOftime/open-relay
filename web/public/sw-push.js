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

  const targetUrl = event.notification?.data?.navigation_url || '/'

  event.waitUntil(
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then((clients) => {
      if (clients.length > 0 && 'navigate' in clients[0]) {
        return clients[0].navigate(targetUrl).then((client) => client?.focus?.())
      }
      return self.clients.openWindow(targetUrl)
    })
  )
})
