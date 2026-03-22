import type { SessionNotificationData } from '@/api/types'

const DEFAULT_NOTIFICATION_TITLE = 'Open Relay notification'
const DEFAULT_NOTIFICATION_TAG = 'open-relay-session-notification'
export const NOTIFICATION_TARGET_PARAM = 'open-relay-target'

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

function notificationLaunchPath(
  payload: Pick<SessionNotificationData, 'navigation_url' | 'node'>
): string {
  const targetPath = notificationNavigationPath(payload)
  if (!targetPath.startsWith('/')) return targetPath
  if (targetPath === '/') return '/'
  return `/?${NOTIFICATION_TARGET_PARAM}=${encodeURIComponent(targetPath)}`
}

export function notificationNavigationUrl(
  payload: Pick<SessionNotificationData, 'navigation_url' | 'node'>,
  origin?: string
): string {
  const path = notificationNavigationPath(payload)
  return origin ? new URL(path, origin).toString() : path
}

export function notificationLaunchUrl(
  payload: Pick<SessionNotificationData, 'navigation_url' | 'node'>,
  origin?: string
): string {
  const path = notificationLaunchPath(payload)
  return origin ? new URL(path, origin).toString() : path
}

export function notificationLaunchTargetFromUrl(currentUrl: string): string | null {
  const baseOrigin =
    typeof window !== 'undefined' && window.location?.origin
      ? window.location.origin
      : 'http://localhost'
  const url = new URL(currentUrl, baseOrigin)
  const target = url.searchParams.get(NOTIFICATION_TARGET_PARAM)?.trim()
  if (!target || !target.startsWith('/')) return null
  return target
}
