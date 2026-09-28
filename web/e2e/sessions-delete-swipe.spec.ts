import { test, expect, type Page } from '@playwright/test'

async function swipe(
  page: Page,
  selector: string,
  fromX: number,
  toX: number,
  fromY = 240,
  toY = 247
) {
  await page.evaluate(
    ({ selector, fromX, toX, fromY, toY }) => {
      const target = document.querySelector(selector)
      if (!target) throw new Error(`Missing swipe target: ${selector}`)
      const start = new Touch({ identifier: 1, target, clientX: fromX, clientY: fromY })
      const end = new Touch({ identifier: 1, target, clientX: toX, clientY: toY })
      target.dispatchEvent(
        new TouchEvent('touchstart', {
          bubbles: true,
          cancelable: true,
          touches: [start],
          targetTouches: [start],
          changedTouches: [start],
        })
      )
      target.dispatchEvent(
        new TouchEvent('touchend', {
          bubbles: true,
          cancelable: true,
          touches: [],
          targetTouches: [],
          changedTouches: [end],
        })
      )
    },
    { selector, fromX, toX, fromY, toY }
  )
}

test('mobile node swipes respect scrolling and deletion confirms force only for active sessions', async ({
  page,
}) => {
  await page.setViewportSize({ width: 375, height: 812 })
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
  await page.route('**/api/nodes', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: '[{"name":"worker-a","connected":true},{"name":"worker-b","connected":true}]',
    })
  )
  await page.route('**/api/push/public-key', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '{"public_key":null}' })
  )
  const makeSession = (node: string | null, status: string) => ({
    id: node ? `${node}-session` : 'local-session',
    title: node ? `${node} session` : 'Local session',
    node,
    status,
    tags: [],
    command: 'bash',
    args: [],
    pid: 42,
    created_at: new Date().toISOString(),
    started_at: null,
    ended_at: null,
    cwd: '/tmp',
    input_needed: false,
    notifications_enabled: true,
    last_total_bytes: 0,
  })
  const removed = new Set<string>()
  await page.route('**/api/sessions?**', (route) => {
    const node = new URL(route.request().url()).searchParams.get('node')
    const source = makeSession(node, node === 'worker-a' ? 'running' : 'stopped')
    const items = removed.has(source.id) ? [] : [source]
    return route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ items, total: items.length, offset: 0, limit: 15 }),
    })
  })
  const requests: Array<{ id: string; node: string | null; force: string | null }> = []
  await page.route('**/api/sessions/*', (route) => {
    const request = route.request()
    expect(request.method()).toBe('DELETE')
    const url = new URL(request.url())
    const id = decodeURIComponent(url.pathname.split('/').at(-1) ?? '')
    const node = url.searchParams.get('node')
    const force = url.searchParams.get('force')
    requests.push({ id, node, force })
    if (id === 'worker-a-session' && requests.length === 1) {
      return route.fulfill({
        status: 503,
        contentType: 'application/json',
        body: '{"error":"disk busy"}',
      })
    }
    removed.add(id)
    return route.fulfill({ status: 200, contentType: 'application/json', body: '{"removed":true}' })
  })

  await page.goto('/')
  const list = page.getByTestId('mobile-session-list')
  await expect(list.getByText('Local session')).toBeVisible()
  await swipe(page, '[data-testid="mobile-session-list"]', 250, 110)
  await expect(page).toHaveURL(/node=worker-a/)
  await expect(list.getByText('worker-a session')).toBeVisible()
  await swipe(page, '[data-node-swipe-ignore]', 250, 110)
  await expect(page).toHaveURL(/node=worker-a/)
  await swipe(page, '[data-testid="mobile-session-list"]', 250, 180, 110, 300)
  await expect(page).toHaveURL(/node=worker-a/)
  await swipe(page, '[data-testid="mobile-session-list"]', 250, 110)
  await expect(page).toHaveURL(/node=worker-b/)
  await swipe(page, '[data-testid="mobile-session-list"]', 110, 250)
  await expect(page).toHaveURL(/node=worker-a/)

  await list.getByRole('button', { name: 'Delete session' }).click()
  let dialog = page.getByRole('alertdialog')
  await expect(dialog).toContainText('The running process will be killed first.')
  await dialog.getByRole('button', { name: 'Cancel' }).click()
  expect(requests).toHaveLength(0)
  await list.getByRole('button', { name: 'Delete session' }).click()
  await dialog.getByRole('button', { name: 'Delete session' }).click()
  await expect(dialog.getByRole('alert')).toContainText('disk busy')
  await expect(list.getByText('worker-a session')).toBeVisible()
  await dialog.getByRole('button', { name: 'Delete session' }).click()
  await expect(dialog).toHaveCount(0)
  await expect(list.getByText('worker-a session')).toHaveCount(0)
  expect(requests.slice(0, 2)).toEqual([
    { id: 'worker-a-session', node: 'worker-a', force: 'true' },
    { id: 'worker-a-session', node: 'worker-a', force: 'true' },
  ])

  await swipe(page, '[data-testid="mobile-session-list"]', 110, 250)
  await expect(list.getByText('Local session')).toBeVisible()
  await list.getByRole('button', { name: 'Delete session' }).click()
  dialog = page.getByRole('alertdialog')
  await expect(dialog).not.toContainText('running process')
  await dialog.getByRole('button', { name: 'Delete session' }).click()
  await expect(dialog).toHaveCount(0)
  expect(requests.at(-1)).toEqual({ id: 'local-session', node: null, force: null })

  // The desktop table exposes the same confirmed, non-force action.
  await page.setViewportSize({ width: 1280, height: 800 })
  const selector = page.locator('header').getByRole('combobox').last()
  await selector.click()
  await page.getByRole('option', { name: 'worker-b' }).click()
  await expect(page.locator('table').getByText('worker-b session')).toBeVisible()
  await page.locator('table').getByRole('button', { name: 'Delete session' }).click()
  dialog = page.getByRole('alertdialog')
  await dialog.getByRole('button', { name: 'Delete session' }).click()
  await expect(dialog).toHaveCount(0)
  expect(requests.at(-1)).toEqual({ id: 'worker-b-session', node: 'worker-b', force: null })
})
