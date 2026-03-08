self.addEventListener('push', (event) => {
  let payload = {
    summary: 'Open Relay notification',
    body: '',
    session_ids: [],
  }

  if (event.data) {
    try {
      payload = { ...payload, ...event.data.json() }
    } catch {
      payload = { ...payload, body: event.data.text() }
    }
  }

  const title = payload.summary || 'Open Relay notification'
  const options = {
    body: payload.body || '',
    tag: payload.session_ids?.[0] || 'open-relay-session-notification',
    data: payload,
  }

  event.waitUntil(self.registration.showNotification(title, options))
})

self.addEventListener('notificationclick', (event) => {
  event.notification.close()

  const sessionId = event.notification?.data?.session_ids?.[0]
  const targetUrl = sessionId ? `/session/${encodeURIComponent(sessionId)}` : '/'

  event.waitUntil(
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then((clients) => {
      if (clients.length > 0 && 'navigate' in clients[0]) {
        return clients[0].navigate(targetUrl).then((client) => client?.focus?.())
      }
      return self.clients.openWindow(targetUrl)
    })
  )
})
