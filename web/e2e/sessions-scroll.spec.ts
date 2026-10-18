import { test, expect, type Page } from '@playwright/test'

/**
 * Coming back to the sessions list should land on the rows the user left, not
 * at the top: the list saves its scroll offset in `sessionStorage` and
 * restores it once the rows are rendered again.
 *
 * The mobile cards and the desktop table are separate scrollports, so each
 * gets a round trip through a session detail page. The file runs at a phone
 * viewport by construction, like `sessions-group-collapse.spec.ts`; the
 * desktop case widens the viewport.
 */
test.use({ viewport: { width: 375, height: 812 } })

/** The card/row whose Logs link the round trip goes through. */
const TARGET_ID = 's-20'

function makeSession(id: string, title: string, ageSeconds: number) {
  return {
    id,
    title,
    tags: [],
    command: 'bash',
    args: [],
    pid: 1234,
    status: 'stopped',
    created_at: new Date(Date.now() - ageSeconds * 1000).toISOString(),
    started_at: null,
    ended_at: null,
    cwd: '/tmp',
    input_needed: false,
    notifications_enabled: true,
    node: null,
    last_total_bytes: 0,
    last_output_epoch: null,
  }
}

/**
 * The list and the detail page it links to. The detail page needs the live
 * events stream to open or the header sits in "Reconnecting" and re-fetches
 * the summary, so `EventSource` is replaced rather than routed.
 */
async function mockSessions(page: Page, items: ReturnType<typeof makeSession>[]) {
  await page.addInitScript(() => {
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
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ auth_required: false }),
    })
  )
  await page.route('**/api/nodes', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '[]' })
  )
  await page.route('**/api/push/public-key', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ public_key: null }),
    })
  )
  await page.route('**/api/sessions/*', (route) => {
    const sessionId = new URL(route.request().url()).pathname.split('/').pop() ?? TARGET_ID
    return route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(makeSession(sessionId, 'scroll target', 1)),
    })
  })
  await page.route('**/api/sessions/*/logs**', (route) => {
    if (new URL(route.request().url()).pathname.endsWith('/tail')) {
      return route.fulfill({ status: 200, body: '', headers: { 'x-log-resizes': '[]' } })
    }
    return route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ offset: 0, chunks: [], total: 0, resizes: [] }),
    })
  })
  // The list is the only route that answers `/api/sessions` itself; everything
  // deeper falls through to the detail and logs handlers above.
  await page.route('**/api/sessions**', (route) => {
    if (new URL(route.request().url()).pathname !== '/api/sessions') {
      return route.fallback()
    }
    return route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ items, total: items.length, offset: 0, limit: 15 }),
    })
  })
}

function scrollSessions() {
  return Array.from({ length: 30 }, (_, index) =>
    makeSession(`s-${index}`, `scroll session ${String(index).padStart(2, '0')}`, index)
  )
}

test('the card list comes back to its offset after a session round trip', async ({ page }) => {
  await mockSessions(page, scrollSessions())
  await page.goto('/')

  const list = page.getByTestId('mobile-session-list')
  const card = page
    .getByTestId('mobile-session-content')
    .getByTestId('session-card')
    .filter({ hasText: 'scroll session 20' })
  await card.scrollIntoViewIfNeeded()
  const before = await list.evaluate((element) => element.scrollTop)
  expect(before).toBeGreaterThan(200)

  await card.getByRole('link', { name: 'Logs' }).click()
  await expect(page).toHaveURL(new RegExp(`/session/${TARGET_ID}\\?mode=logs$`))
  await expect(page.getByRole('button', { name: 'Back to sessions' })).toBeVisible()

  await page.getByRole('button', { name: 'Back to sessions' }).click()
  await expect(page).toHaveURL('/')
  await expect(card).toBeInViewport()

  await expect
    .poll(() => list.evaluate((element) => element.scrollTop))
    .toBeGreaterThanOrEqual(Math.floor(before) - 1)
  expect(await list.evaluate((element) => element.scrollTop)).toBeLessThanOrEqual(before + 1)
})

test('the desktop table comes back to its offset after a session round trip', async ({ page }) => {
  await mockSessions(page, scrollSessions())
  await page.setViewportSize({ width: 1280, height: 800 })
  await page.goto('/')

  const list = page.getByTestId('desktop-session-list')
  const row = page.getByRole('row').filter({ hasText: 'scroll session 20' })
  await row.scrollIntoViewIfNeeded()
  const before = await list.evaluate((element) => element.scrollTop)
  expect(before).toBeGreaterThan(200)

  await row.getByRole('link', { name: 'Logs' }).click()
  await expect(page).toHaveURL(new RegExp(`/session/${TARGET_ID}\\?mode=logs$`))

  await page.getByRole('button', { name: 'Back to sessions' }).click()
  await expect(page).toHaveURL('/')
  await expect(row).toBeInViewport()

  await expect
    .poll(() => list.evaluate((element) => element.scrollTop))
    .toBeGreaterThanOrEqual(Math.floor(before) - 1)
  expect(await list.evaluate((element) => element.scrollTop)).toBeLessThanOrEqual(before + 1)
})
