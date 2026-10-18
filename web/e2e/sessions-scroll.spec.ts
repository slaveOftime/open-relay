import { test, expect, type Page, type Response } from '@playwright/test'

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

/** The list fetch, as opposed to the per-session fetches that share the prefix. */
function isSessionListRequest(response: Response) {
  return new URL(response.url()).pathname === '/api/sessions'
}

/** Whether the rendered page still shows `text`; a blank reload takes it away. */
function pageHasText(page: Page, text: string) {
  return page.evaluate((needle) => document.body.innerText.includes(needle), text)
}

/**
 * Click the first visible element matching `selector` whose text or aria-label
 * contains `label`.
 *
 * Playwright's own clicks wait for actionability by polling animation frames,
 * and the reload race test below holds frames on purpose, so clicks inside the
 * held window are dispatched through the DOM instead. React listens at the
 * root, so a bubbling `MouseEvent` reaches the handler a real click would.
 */
async function clickByText(page: Page, selector: string, label: string) {
  await page.evaluate(
    ({ selector, label: needle }) => {
      const target = [...document.querySelectorAll(selector)].find((node) => {
        const visible = node.getClientRects().length > 0
        const named =
          (node.textContent?.includes(needle) ?? false) ||
          (node.getAttribute('aria-label')?.includes(needle) ?? false)
        return visible && named
      })
      target?.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))
    },
    { selector, label }
  )
}

/**
 * Hold every animation frame until `releaseAnimationFrames` runs them.
 *
 * The restore lands on a frame, so a test that needs to interrupt it before it
 * lands cannot let frames run. Frames the app queues while held (sparklines,
 * xterm, the delayed terminal disposal of the page left behind) simply wait
 * and run at the release.
 */
async function holdAnimationFrames(page: Page) {
  await page.evaluate(() => {
    const held = new Map<number, FrameRequestCallback>()
    let nextId = 1
    const originalRaf = window.requestAnimationFrame.bind(window)
    const originalCancel = window.cancelAnimationFrame.bind(window)
    window.requestAnimationFrame = (callback: FrameRequestCallback) => {
      const id = nextId++
      held.set(id, callback)
      return id
    }
    window.cancelAnimationFrame = (id: number) => {
      held.delete(id)
    }
    ;(window as unknown as { __olyFlushFrames?: () => void }).__olyFlushFrames = () => {
      window.requestAnimationFrame = originalRaf
      window.cancelAnimationFrame = originalCancel
      for (const [id, callback] of [...held]) {
        held.delete(id)
        callback(performance.now())
      }
    }
  })
}

async function releaseAnimationFrames(page: Page) {
  await page.evaluate(() => {
    ;(window as unknown as { __olyFlushFrames?: () => void }).__olyFlushFrames?.()
  })
}

/**
 * Coming back to the list and hitting a reload before the restore lands must
 * still end at the saved offset. A foreground reload hides the rows while it
 * runs: `hasRows` dips back to false (the pending restore frame is cancelled),
 * the content collapse clamps the list to the top, and only the rows coming
 * back can put the offset in place. Frames are held from before that dip until
 * the reload's rows return, which makes the window deterministic instead of a
 * 16ms race.
 */
test('the offset survives a reload that interrupts the restore', async ({ page }) => {
  await mockSessions(page, scrollSessions())
  // Delay the list fetches so the reload's blank state is a window the test
  // can observe rather than a blink it has to outrun.
  await page.route('**/api/sessions**', async (route) => {
    if (new URL(route.request().url()).pathname !== '/api/sessions') return route.fallback()
    await new Promise((resolve) => setTimeout(resolve, 300))
    return route.fallback()
  })
  await page.setViewportSize({ width: 1280, height: 800 })
  await page.goto('/')

  const list = page.getByTestId('desktop-session-list')
  const row = page.getByRole('row').filter({ hasText: 'scroll session 20' })
  await row.scrollIntoViewIfNeeded()
  const before = await list.evaluate((element) => element.scrollTop)
  expect(before).toBeGreaterThan(200)

  await row.getByRole('link', { name: 'Logs' }).click()
  await expect(page).toHaveURL(new RegExp(`/session/${TARGET_ID}\\?mode=logs$`))

  await holdAnimationFrames(page)

  const backResponse = page.waitForResponse(isSessionListRequest)
  await clickByText(page, 'button', 'Back to sessions')
  await backResponse
  await expect.poll(() => pageHasText(page, 'scroll session 20')).toBe(true)

  const reloadResponse = page.waitForResponse(isSessionListRequest)
  await clickByText(page, 'span', 'Open Relay')
  await expect.poll(() => pageHasText(page, 'scroll session 20')).toBe(false)
  await reloadResponse
  await expect.poll(() => pageHasText(page, 'scroll session 20')).toBe(true)

  await releaseAnimationFrames(page)

  await expect
    .poll(() => list.evaluate((element) => element.scrollTop))
    .toBeGreaterThanOrEqual(Math.floor(before) - 1)
  expect(await list.evaluate((element) => element.scrollTop)).toBeLessThanOrEqual(before + 1)
})
