import type { SessionNotificationData } from '@/api/types'

const DEFAULT_NOTIFICATION_TITLE = 'Open Relay notification'
const DEFAULT_NOTIFICATION_TAG = 'open-relay-session-notification'
export const NOTIFICATION_TARGET_PARAM = 'open-relay-target'
export const NOTIFICATION_CLICK_MESSAGE = 'open-relay:notification-click'

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
  payload: Pick<SessionNotificationData, 'navigation_url' | 'node'>,
  origin?: string
): string {
  const targetPath = notificationNavigationPath(payload)
  const normalizedTarget = normalizeNotificationTarget(targetPath, origin)
  if (!normalizedTarget) return targetPath
  const normalizedUrl = new URL(normalizedTarget, notificationBaseOrigin(origin))
  const usesInAppLaunch =
    normalizedUrl.pathname === '/' || normalizedUrl.pathname.startsWith('/session/')
  if (!usesInAppLaunch) return targetPath
  if (normalizedTarget === '/') return '/'
  return `/?${NOTIFICATION_TARGET_PARAM}=${encodeURIComponent(normalizedTarget)}`
}

function notificationBaseOrigin(origin?: string): string {
  if (origin) return origin
  if (typeof window !== 'undefined' && window.location?.origin) return window.location.origin
  return 'http://localhost'
}

export function normalizeNotificationTarget(target: string, origin?: string): string | null {
  const baseOrigin = notificationBaseOrigin(origin)
  const url = new URL(target, baseOrigin)
  if (url.origin !== baseOrigin) return null
  return `${url.pathname}${url.search}${url.hash}`
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
  const path = notificationLaunchPath(payload, origin)
  return origin ? new URL(path, origin).toString() : path
}

export function notificationLaunchTargetFromUrl(currentUrl: string): string | null {
  const baseOrigin = notificationBaseOrigin()
  const url = new URL(currentUrl, baseOrigin)
  const target = url.searchParams.get(NOTIFICATION_TARGET_PARAM)?.trim()
  if (!target) return null
  return normalizeNotificationTarget(target, baseOrigin)
}

function readNotificationClickTarget(data: unknown): string | null {
  if (!data || typeof data !== 'object') return null
  const type = Reflect.get(data, 'type')
  const target = Reflect.get(data, 'target')
  if (type !== NOTIFICATION_CLICK_MESSAGE || typeof target !== 'string') return null
  return target
}

export function notificationClickMessageTarget(data: unknown, origin?: string): string | null {
  const target = readNotificationClickTarget(data)
  if (!target) return null
  return normalizeNotificationTarget(target, origin)
}
