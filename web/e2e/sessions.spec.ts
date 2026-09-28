import { test, expect } from '@playwright/test'

test('renders sessions page with mocked API data', async ({ page }) => {
  const item = {
    id: '1234567-89ab-cdef-0123-456789abcdef',
    title: 'demo session',
    tags: [],
    command: 'bash',
    args: [],
    pid: 1234,
    status: 'running',
    age: '2m',
    created_at: new Date().toISOString(),
    cwd: '/tmp',
    input_needed: false,
    node: null,
    last_total_bytes: 0,
  }
  await page.route('**/api/auth/status', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ auth_required: false }),
    })
  })

  await page.route('**/api/nodes', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify([]),
    })
  })

  await page.route('**/api/push/public-key', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ public_key: null }),
    })
  })

  await page.route('**/api/push/subscriptions', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ ok: true, deleted: false }),
    })
  })

  await page.route('**/api/sessions/events**', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'text/event-stream',
      body: `event: snapshot\ndata: ${JSON.stringify([item])}\n\n`,
    })
  })

  await page.route('**/api/sessions**', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        items: [item],
        total: 1,
        offset: 0,
        limit: 15,
      }),
    })
  })

  await page.goto('/')

  await expect(page.locator('header').first()).toContainText('Open Relay')
  await expect(page.locator('table').getByText('demo session')).toBeVisible()
})

test('switching nodes ignores old list responses and keeps the live indicator stable', async ({
  page,
}) => {
  await page.addInitScript(() => {
    // Keep the mocked stream open while the table and its activity cells render.
    class OpenEventSource extends EventTarget {
      onopen: ((event: Event) => void) | null = null
      onerror: ((event: Event) => void) | null = null
      constructor(url: string) {
        super()
        void url
        setTimeout(() => this.onopen?.(new Event('open')), 0)
      }
      close() {}
    }
    Object.defineProperty(window, 'EventSource', { value: OpenEventSource })
  })

  await page.route('**/api/auth/status', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '{"auth_required":false}' })
  )
  await page.route('**/api/nodes', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify([{ name: 'worker-a' }, { name: 'worker-b' }]),
    })
  )
  await page.route('**/api/push/public-key', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '{"public_key":null}' })
  )

  const session = (title: string, node: string | null) => ({
    id: `${title.replaceAll(' ', '-')}-id`,
    title,
    tags: [],
    command: 'bash',
    args: [],
    pid: 1234,
    status: 'running',
    created_at: new Date().toISOString(),
    cwd: '/tmp',
    input_needed: false,
    node,
    last_total_bytes: 0,
  })
  let releaseLocal: (() => void) | undefined
  let releaseWorkerA: (() => void) | undefined
  await page.route('**/api/sessions?**', async (route) => {
    const node = new URL(route.request().url()).searchParams.get('node')
    if (node === null)
      await new Promise<void>((resolve) => {
        releaseLocal = resolve
      })
    if (node === 'worker-a')
      await new Promise<void>((resolve) => {
        releaseWorkerA = resolve
      })
    const title = node === null ? 'local session' : `${node} session`
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ items: [session(title, node)], total: 1, offset: 0, limit: 15 }),
    })
  })

  await page.setViewportSize({ width: 1280, height: 800 })
  await page.goto('/')
  await expect.poll(() => Boolean(releaseLocal)).toBe(true)
  const nodeSelector = page.locator('header').getByRole('combobox').last()
  await expect(nodeSelector).toBeVisible()
  await nodeSelector.click()
  await page.getByRole('option', { name: 'worker-a' }).click()
  await expect.poll(() => Boolean(releaseWorkerA)).toBe(true)
  await nodeSelector.click()
  await page.getByRole('option', { name: 'worker-b' }).click()
  await expect(page.locator('table').getByText('worker-b session')).toBeVisible()
  await expect(page.getByText('Live', { exact: true })).toBeVisible()

  const workerAResponse = page.waitForResponse(
    (response) =>
      response.url().includes('/api/sessions?') &&
      new URL(response.url()).searchParams.get('node') === 'worker-a'
  )
  const localResponse = page.waitForResponse(
    (response) =>
      response.url().includes('/api/sessions?') && !new URL(response.url()).searchParams.has('node')
  )
  releaseWorkerA?.()
  releaseLocal?.()
  await Promise.all([workerAResponse, localResponse])
  // Let both fetch continuations and their React updates commit before asserting.
  await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => resolve())))
  await expect(page.locator('table').getByText('worker-b session')).toBeVisible()
  await expect(page.locator('table').getByText('worker-a session')).toHaveCount(0)
  await expect(page.locator('table').getByText('local session')).toHaveCount(0)
  await expect(page.getByText('Live', { exact: true })).toBeVisible()

  await page.setViewportSize({ width: 375, height: 812 })
  await expect(page.getByText('Live', { exact: true })).toBeVisible()
  await expect(page.getByText('worker-b session').first()).toBeVisible()
})
