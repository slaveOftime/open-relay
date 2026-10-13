import { test, expect, type Page, type Route } from '@playwright/test'

/**
 * The remote-node counterpart of session-stop-optimistic.spec.ts.
 *
 * A remote page never sees the daemon's own `session_updated` summaries, so the
 * optimistic `stopping` claim has to survive a background re-pull that is still
 * answered with the pre-stop `running` status. The `session_created` frame
 * below is the cheapest way to force that re-pull while the stop request is
 * still open.
 */

const SESSION_ID = 'bbbbbbbb-1111-2222-3333-444444444444'
const NODE = 'worker-a'

type Status = 'running' | 'stopped'

/** The status the stubbed list reports; flips once the stop request answers. */
let listStatus: Status = 'running'

function summary(status: Status = listStatus) {
  return {
    id: SESSION_ID,
    title: 'remote session',
    tags: [],
    command: 'bash',
    args: [],
    pid: 1234,
    status,
    created_at: new Date().toISOString(),
    started_at: null,
    ended_at: null,
    cwd: '/tmp',
    input_needed: false,
    notifications_enabled: false,
    node: NODE,
    last_total_bytes: 0,
    last_output_epoch: null,
  }
}

async function stubApi(page: Page) {
  await page.addInitScript(() => {
    class OpenEventSource extends EventTarget {
      static latest: OpenEventSource | null = null
      onopen: ((event: Event) => void) | null = null
      onerror: ((event: Event) => void) | null = null
      constructor(url: string) {
        super()
        void url
        OpenEventSource.latest = this
        setTimeout(() => this.onopen?.(new Event('open')), 0)
      }
      close() {}
    }
    Object.defineProperty(window, 'EventSource', { value: OpenEventSource })
    ;(window as Window & { emitSse?: (event: string, data: unknown) => void }).emitSse = (
      event,
      data
    ) =>
      OpenEventSource.latest?.dispatchEvent(new MessageEvent(event, { data: JSON.stringify(data) }))
  })
  await page.route('**/api/auth/status', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '{"auth_required":false}' })
  )
  await page.route('**/api/nodes', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify([{ name: NODE, status: 'online' }]),
    })
  )
  await page.route('**/api/push/public-key', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '{"public_key":null}' })
  )
  await page.route('**/api/push/subscriptions', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '{"ok":true}' })
  )
  await page.route('**/api/sessions/events**', (route) =>
    route.fulfill({ status: 200, contentType: 'text/event-stream', body: '' })
  )
  await page.route('**/api/sessions?**', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ items: [summary()], total: 1, offset: 0, limit: 15 }),
    })
  )
}

/** A stop request that only answers once `release()` is called. */
function deferredStop(page: Page) {
  const state = { calls: 0, release: undefined as undefined | (() => void) }
  const handler = async (route: Route) => {
    state.calls += 1
    await new Promise<void>((resolve) => {
      state.release = resolve
    })
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ stopped: true }),
    })
  }
  page.route(`**/api/sessions/**/stop?**`, handler)
  page.route(`**/api/sessions/**/stop`, handler)
  return state
}

test('the remote row keeps Stopping across a background re-pull', async ({ page }) => {
  await stubApi(page)
  const terminate = deferredStop(page)

  await page.setViewportSize({ width: 1280, height: 800 })
  await page.goto(`/?node=${NODE}`)
  const row = page.locator('table tbody tr').first()
  await expect(row.getByText('Running', { exact: true })).toBeVisible()

  // The id link button, then Stop, Kill, notifications, …
  await row.getByRole('button').nth(1).click()
  await page.getByRole('button', { name: 'Stop session' }).click()
  await expect.poll(() => terminate.calls).toBe(1)
  await expect(row.getByText('Stopping', { exact: true })).toBeVisible()
  await expect(row.getByText('Running', { exact: true })).toHaveCount(0)

  // A new session on this node re-pulls the list; the daemon still reports
  // `running` because the drain has not finished.
  await page.evaluate((event: string) => window.emitSse?.(event, {}), 'session_created')
  await expect.poll(() => terminate.calls).toBe(1)
  await expect(row.getByText('Stopping', { exact: true })).toBeVisible()
  await expect(row.getByText('Running', { exact: true })).toHaveCount(0)

  // Once the request settles the list reports the terminal status, and the
  // post-settle re-pull shows it.
  listStatus = 'stopped'
  terminate.release?.()
  await expect(row.getByText('Stopped', { exact: true })).toBeVisible()
})
