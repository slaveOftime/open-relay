import type { SessionNotificationData } from '@/api/types'

const DEFAULT_NOTIFICATION_TITLE = 'Open Relay notification'
const DEFAULT_NOTIFICATION_TAG = 'open-relay-session-notification'

export function notificationTitle(payload: Pick<SessionNotificationData, 'title'>): string {
  return payload.title.trim() || DEFAULT_NOTIFICATION_TITLE
}

export function notificationBody(
  payload: Pick<SessionNotificationData, 'description' | 'body'>
): string {
  const description = payload.description.trim()
  const body = payload.body.trim()

  if (description && body) return `${description}\n${body}`
  return description || body
}

export function notificationTag(payload: Pick<SessionNotificationData, 'session_ids'>): string {
  return payload.session_ids[0] ?? DEFAULT_NOTIFICATION_TAG
}

function notificationNavigationPath(
  payload: Pick<SessionNotificationData, 'navigation_url' | 'node'>
): string {
  const base = payload.navigation_url?.trim() || '/'
  const node = payload.node?.trim()
  if (!node || !base.startsWith('/')) return base
  return `${base}${base.includes('?') ? '&' : '?'}node=${encodeURIComponent(node)}`
}

export function notificationNavigationUrl(
  payload: Pick<SessionNotificationData, 'navigation_url' | 'node'>,
  origin?: string
): string {
  const path = notificationNavigationPath(payload)
  return origin ? new URL(path, origin).toString() : path
}
