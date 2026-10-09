/**
 * Nodes and push subscriptions.
 *
 * Both are server-side registries the client reads or registers with, rather
 * than session payloads, so they live together and apart from
 * ./client-sessions.ts.
 */

import type { NodeSummary, PushSubscriptionInput } from './types'
import { BASE, req } from './client-http'

export function fetchNodes(): Promise<NodeSummary[]> {
  return req<NodeSummary[]>(`${BASE}/nodes`)
}

export function fetchPushPublicKey(): Promise<{ public_key: string | null }> {
  return req<{ public_key: string | null }>(`${BASE}/push/public-key`)
}

export function upsertPushSubscription(
  subscription: PushSubscriptionInput
): Promise<{ ok: boolean }> {
  return req<{ ok: boolean }>(`${BASE}/push/subscriptions`, {
    method: 'POST',
    body: JSON.stringify(subscription),
  })
}

export function deletePushSubscription(
  endpoint: string
): Promise<{ ok: boolean; deleted: boolean }> {
  return req<{ ok: boolean; deleted: boolean }>(`${BASE}/push/subscriptions`, {
    method: 'DELETE',
    body: JSON.stringify({ endpoint }),
  })
}
