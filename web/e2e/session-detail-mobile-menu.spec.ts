import { test, expect } from '@playwright/test'

test('session detail mobile actions have thumb-sized targets and remain usable on short screens', async ({
  page,
}) => {
  await page.setViewportSize({ width: 375, height: 667 })
  await page.addInitScript(() => {
    class OpenEventSource extends EventTarget {
      onopen: ((event: Event) => void) | null = null
      constructor() {
        super()
        setTimeout(() => this.onopen?.(new Event('open')), 0)
      }
      close() {}
    }
    Object.defineProperty(window, 'EventSource', { value: OpenEventSource })
  })
  await page.route('**/api/auth/status', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '{"auth_required":false}' })
  )
  await page.route('**/api/sessions/mobile-session', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        id: 'mobile-session',
        title: 'Mobile test',
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
        notifications_enabled: true,
        node: null,
        last_total_bytes: 0,
      }),
    })
  )
  await page.route('**/api/sessions/mobile-session/logs**', (route) => {
    if (new URL(route.request().url()).pathname.endsWith('/tail')) {
      return route.fulfill({ status: 200, body: '', headers: { 'x-log-resizes': '[]' } })
    }
    return route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ offset: 0, chunks: [], total: 0, resizes: [] }),
    })
  })

  await page.goto('/session/mobile-session?mode=logs')
  const trigger = page.getByRole('button', { name: 'Session actions' })
  await expect(trigger).toBeVisible()
  const triggerBox = await trigger.boundingBox()
  expect(triggerBox?.width).toBeGreaterThanOrEqual(44)
  expect(triggerBox?.height).toBeGreaterThanOrEqual(44)

  await trigger.click()
  const items = page.getByRole('menuitem')
  await expect(items).toHaveCount(5)
  for (const item of await items.all()) {
    const box = await item.boundingBox()
    expect(box?.height).toBeGreaterThanOrEqual(44)
  }
  await items.getByText('Run Again').click()
  await expect(page.getByRole('dialog')).toBeVisible()
  await page.getByRole('button', { name: 'Cancel' }).click()

  await page.setViewportSize({ width: 375, height: 320 })
  await trigger.click()
  await items.last().scrollIntoViewIfNeeded()
  await expect(items.last()).toBeVisible()
  await page.keyboard.press('Escape')

  await page.setViewportSize({ width: 1024, height: 768 })
  await expect(trigger).toBeHidden()
  await expect(page.getByRole('button', { name: 'Refresh' })).toBeVisible()
})
