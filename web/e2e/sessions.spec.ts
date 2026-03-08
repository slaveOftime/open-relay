import { test, expect } from '@playwright/test'

test('renders sessions page with mocked API data', async ({ page }) => {
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
      body: 'event: snapshot\ndata: []\n\n',
    })
  })

  await page.route('**/api/sessions**', async (route) => {
    const now = new Date().toISOString()
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        items: [
          {
            id: '1234567-89ab-cdef-0123-456789abcdef',
            title: 'demo session',
            command: 'bash',
            args: [],
            pid: 1234,
            status: 'running',
            age: '2m',
            created_at: now,
            cwd: '/tmp',
            input_needed: false,
          },
        ],
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
