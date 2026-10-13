/**
 * Auth API: token storage lives in ./client-http.ts, this module owns the
 * login/logout/status endpoints.
 */

import type { AuthStatus, LoginResponse } from './types'
import { TooManyAttemptsError } from './types'
import { BASE, clearToken, getToken, req } from './client-http'

export function getAuthStatus(): Promise<AuthStatus> {
  return req<AuthStatus>(`${BASE}/auth/status`)
}

export async function login(password: string): Promise<LoginResponse> {
  const res = await fetch(`${BASE}/auth/login`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    // Keep the default same-origin credentials mode so the browser stores the
    // HttpOnly auth cookie from the Set-Cookie response header. Without it the
    // cookie is dropped and document-level navigations (proxy apps) 401.
    body: JSON.stringify({ password }),
  })
  if (res.status === 429) {
    const body = await res.json().catch(() => ({}))
    const secs = body?.retry_after_seconds ?? 900
    throw new TooManyAttemptsError(secs)
  }
  if (!res.ok) {
    const body = await res.json().catch(() => ({}))
    // Re-attach attempts_remaining if present for consumers
    const err = new Error(body?.error ?? `HTTP ${res.status}`) as Error & {
      attemptsRemaining?: number
    }
    err.attemptsRemaining = body?.attempts_remaining
    throw err
  }
  return res.json() as Promise<LoginResponse>
}

export async function logout(): Promise<void> {
  const token = getToken()
  if (!token) return
  await fetch(`${BASE}/auth/logout`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
  }).catch(() => {
    /* best effort */
  })
  clearToken()
}
