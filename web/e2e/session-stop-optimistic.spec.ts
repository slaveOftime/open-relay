import { test, expect, type Page, type Route } from '@playwright/test'

/**
 * The daemon keeps `stopping` to itself: `stop_session`/`kill_session` move the
 * runtime to that state immediately but only publish a summary (and only answer
 * the HTTP request) once the whole drain has finished. These tests hold the
 * request open and assert the UI has already reacted, which is the only way to
 * catch a regression back to "wait for the server to say so".
 */

const SESSION_ID = 'aaaaaaaa-1111-2222-3333-444444444444'

function summary(partial: Record<string, unknown> = {}) {
  return {
    id: SESSION_ID,
    title: 'demo session',
    tags: [],
    command: 'bash',
    args: [],
    pid: 1234,
    status: 'running',
    created_at: new Date().toISOString(),
    started_at: null,
    ended_at: null,
    cwd: '/tmp',
    input_needed: false,
    notifications_enabled: false,
    node: null,
    last_total_bytes: 0,
    last_output_epoch: null,
    ...partial,
  }
}

/** Keeps the mocked SSE stream open for the duration of a test. */
async function stubEventSource(page: Page) {
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
  })
}

async function stubApi(page: Page) {
  await page.route('**/api/auth/status', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '{"auth_required":false}' })
  )
  await page.route('**/api/nodes', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '[]' })
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
}

async function stubList(page: Page) {
  await page.route('**/api/sessions?**', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ items: [summary()], total: 1, offset: 0, limit: 15 }),
    })
  )
}

/** Stubs the endpoints the detail page needs in logs mode (no attach socket). */
async function stubDetail(page: Page) {
  await page.route(`**/api/sessions/${SESSION_ID}`, (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(summary()),
    })
  )
  await page.route(`**/api/sessions/${SESSION_ID}/logs/tail?**`, (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      headers: { 'x-log-resizes': '[]' },
      body: new ArrayBuffer(0),
    })
  )
}

/**
 * The client omits the query string entirely for local sessions, so both shapes
 * have to be intercepted.
 */
function terminateRoutes(page: Page, action: 'stop' | 'kill', handler: (route: Route) => unknown) {
  page.route(`**/api/sessions/**/${action}?**`, handler)
  page.route(`**/api/sessions/**/${action}`, handler)
}

/** A terminate request that only answers once `release()` is called. */
function deferredTerminate(
  page: Page,
  action: 'stop' | 'kill',
  outcome?: { status: number; body: Record<string, unknown> }
) {
  const state = { calls: 0, release: undefined as undefined | (() => void) }
  terminateRoutes(page, action, async (route) => {
    state.calls += 1
    await new Promise<void>((resolve) => {
      state.release = resolve
    })
    await route.fulfill({
      status: outcome?.status ?? 200,
      contentType: 'application/json',
      body: JSON.stringify(outcome?.body ?? (action === 'stop' ? { stopped: true } : { killed: true })),
    })
  })
  return state
}

/**
 * The desktop row's Stop/Kill buttons are icon-only and carry no accessible
 * name, so they are addressed by position within the row's button set: the id
 * link button, Stop, Kill, notifications, pin, Run Again, Delete.
 */
function stopButton(row: ReturnType<Page['locator']>) {
  return row.getByRole('button').nth(1)
}

test('the list row shows Stopping before the stop request answers', async ({ page }) => {
  await stubEventSource(page)
  await stubApi(page)
  await stubList(page)
  const terminate = deferredTerminate(page, 'stop')

  await page.setViewportSize({ width: 1280, height: 800 })
  await page.goto('/')
  const row = page.locator('table tbody tr').first()
  await expect(row.getByText('Running', { exact: true })).toBeVisible()

  await stopButton(row).click()
  await page.getByRole('button', { name: 'Stop session' }).click()

  await expect.poll(() => terminate.calls).toBe(1)
  // The daemon has not answered yet, and never will until we let it.
  await expect(row.getByText('Stopping', { exact: true })).toBeVisible()
  await expect(row.getByText('Running', { exact: true })).toHaveCount(0)

  terminate.release?.()
  await expect(row.getByText('Stopping', { exact: true })).toBeVisible()
})

test('the list row rolls back to Running when the stop request fails', async ({ page }) => {
  await stubEventSource(page)
  await stubApi(page)
  await stubList(page)
  const terminate = deferredTerminate(page, 'stop', {
    status: 502,
    body: { error: 'node unreachable' },
  })

  await page.setViewportSize({ width: 1280, height: 800 })
  await page.goto('/')
  const row = page.locator('table tbody tr').first()
  await expect(row.getByText('Running', { exact: true })).toBeVisible()

  await stopButton(row).click()
  await page.getByRole('button', { name: 'Stop session' }).click()

  await expect.poll(() => terminate.calls).toBe(1)
  await expect(row.getByText('Stopping', { exact: true })).toBeVisible()

  terminate.release?.()
  await expect(page.getByRole('dialog')).toContainText('Failed to stop session')
  await expect(page.getByRole('dialog')).toContainText('node unreachable')
  await expect(row.getByText('Running', { exact: true })).toBeVisible()
  await expect(row.getByText('Stopping', { exact: true })).toHaveCount(0)
})

test('a repeated stop on a stopping session sends no second request', async ({ page }) => {
  await stubEventSource(page)
  await stubApi(page)
  await stubList(page)
  const terminate = deferredTerminate(page, 'stop')

  await page.setViewportSize({ width: 1280, height: 800 })
  await page.goto('/')
  const row = page.locator('table tbody tr').first()
  await expect(row.getByText('Running', { exact: true })).toBeVisible()

  await stopButton(row).click()
  await page.getByRole('button', { name: 'Stop session' }).click()
  await expect.poll(() => terminate.calls).toBe(1)
  await expect(row.getByText('Stopping', { exact: true })).toBeVisible()

  await stopButton(row).click()
  await page.getByRole('button', { name: 'Stop session' }).click()
  await page.waitForTimeout(250)
  expect(terminate.calls).toBe(1)

  terminate.release?.()
})

test('the detail header shows Stopping before the stop request answers', async ({ page }) => {
  await stubEventSource(page)
  await stubApi(page)
  await stubDetail(page)
  const terminate = deferredTerminate(page, 'stop')

  await page.setViewportSize({ width: 1280, height: 800 })
  await page.goto(`/session/${SESSION_ID}?mode=logs`)
  await expect(page.locator('header').getByText('Running', { exact: true })).toBeVisible()

  await page.locator('header').getByRole('button', { name: 'Stop' }).click()
  await page.getByRole('button', { name: 'Stop session' }).click()

  await expect.poll(() => terminate.calls).toBe(1)
  await expect(page.locator('header').getByText('Stopping', { exact: true })).toBeVisible()
  await expect(page.locator('header').getByText('Running', { exact: true })).toHaveCount(0)

  terminate.release?.()
  await expect(page.locator('header').getByText('Stopping', { exact: true })).toBeVisible()
})

test('the detail header rolls back and explains a failed stop', async ({ page }) => {
  await stubEventSource(page)
  await stubApi(page)
  await stubDetail(page)
  const terminate = deferredTerminate(page, 'stop', {
    status: 500,
    body: { error: 'stop failed' },
  })

  await page.setViewportSize({ width: 1280, height: 800 })
  await page.goto(`/session/${SESSION_ID}?mode=logs`)
  await expect(page.locator('header').getByText('Running', { exact: true })).toBeVisible()

  await page.locator('header').getByRole('button', { name: 'Stop' }).click()
  await page.getByRole('button', { name: 'Stop session' }).click()

  await expect.poll(() => terminate.calls).toBe(1)
  await expect(page.locator('header').getByText('Stopping', { exact: true })).toBeVisible()

  terminate.release?.()
  await expect(page.getByText('Failed to stop session: stop failed')).toBeVisible()
  await expect(page.locator('header').getByText('Running', { exact: true })).toBeVisible()
  await page.getByRole('button', { name: 'Dismiss' }).click()
  await expect(page.getByText('Failed to stop session:')).toHaveCount(0)
})