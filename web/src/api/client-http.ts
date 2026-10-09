/**
 * Shared HTTP core: token storage and the request wrapper every API call
 * goes through. Kept separate so the api/ modules can share them without
 * reaching into the client.ts facade.
 */

import { AuthRequiredError } from './types'

export const BASE = '/api'

const TOKEN_KEY = 'oly_auth_token'

/**
 * Token storage (localStorage — persists across tabs and browser restarts).
 * The token itself never expires server-side; it stays valid until the
 * password changes or auth is disabled, so we keep it around. The server also
 * sets a long-lived HttpOnly cookie on login: it authenticates plain document
 * navigations (e.g. /apps/<slug>/ proxy pages) that cannot send headers.
 */

export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY)
}

export function setToken(token: string): void {
  localStorage.setItem(TOKEN_KEY, token)
}

export function clearToken(): void {
  localStorage.removeItem(TOKEN_KEY)
}

/**
 * The session token is no longer accepted: drop it, let the app show the
 * login dialog, and stop the caller from retrying with a dead credential.
 * `never` so callers read as unreachable after the check.
 */
export function rejectUnauthorized(): never {
  clearToken()
  window.dispatchEvent(new CustomEvent('oly:auth-required'))
  throw new AuthRequiredError()
}

export async function req<T>(url: string, init?: RequestInit): Promise<T> {
  const token = getToken()
  const headers = new Headers(init?.headers)
  const isFormData = typeof FormData !== 'undefined' && init?.body instanceof FormData
  if (!isFormData && !headers.has('Content-Type')) {
    headers.set('Content-Type', 'application/json')
  }
  if (token) {
    headers.set('Authorization', `Bearer ${token}`)
  }
  const res = await fetch(url, {
    headers,
    ...init,
  })
  if (res.status === 401) {
    rejectUnauthorized()
  }
  if (!res.ok) {
    const body = await res.json().catch(() => ({}))
    throw new Error(body?.error ?? `HTTP ${res.status}`)
  }
  return res.json() as Promise<T>
}
