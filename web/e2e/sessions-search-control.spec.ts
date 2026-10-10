import { test, expect, type Page } from '@playwright/test'

/**
 * The mobile header's search control. The control is mobile-only, so the file
 * runs at a phone viewport in the desktop Chromium project — pointer events
 * make the tap/hold gestures identical to touch there.
 */
test.use({ viewport: { width: 375, height: 812 } })

function makeSession(id: string, title: string, status: string) {
  return {
    id,
    title,
    tags: [],
    command: 'bash',
    args: [],
    pid: 1234,
    status,
    age: '2m',
    created_at: new Date().toISOString(),
    cwd: '/tmp',
    input_needed: false,
    node: null,
    last_total_bytes: 0,
  }
}

const ALPHA = makeSession('aaaaaaa1-89ab-cdef-0123-456789abcdef', 'alpha bash session', 'running')
const BETA = makeSession('bbbbbbb2-89ab-cdef-0123-456789abcdef', 'beta node session', 'stopped')

async function mockBackend(page: Page) {
  await page.route('**/api/auth/status', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ auth_required: false }),
    })
  )
  await page.route('**/api/nodes', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify([]) })
  )
  await page.route('**/api/push/public-key', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ public_key: null }),
    })
  )
  await page.route('**/api/push/subscriptions', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ ok: true, deleted: false }),
    })
  )
  await page.route('**/api/sessions/events**', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'text/event-stream',
      body: `event: stream_ready\ndata: ${JSON.stringify({ version: 2, nodes: [] })}\n\n`,
    })
  )
  // The list honours the same filters the page sends, so the specs can assert
  // that the search bar and the quick menu actually drive the query.
  await page.route('**/api/sessions**', (route) => {
    if (new URL(route.request().url()).pathname === '/api/sessions/events') {
      void route.fallback()
      return
    }
    const url = new URL(route.request().url())
    let items = [ALPHA, BETA]
    if (url.searchParams.get('search')) items = [ALPHA]
    const status = url.searchParams.get('status')
    if (status) items = items.filter((session) => session.status === status)
    void route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ items, total: items.length, offset: 0, limit: 15 }),
    })
  })
}

const SEARCH_ICON = 'Search sessions'
const SEARCH_INPUT = 'Search sessions by id, title, command, or working directory'

// At 375px the desktop table still holds the same titles (hidden, but matched
// by text queries), so card assertions are scoped to the mobile list.
const list = (page: Page) => page.getByTestId('mobile-session-content')

/** Press and hold the search icon past the 400ms hold threshold. */
async function holdSearchIcon(page: Page) {
  const icon = page.getByRole('button', { name: SEARCH_ICON })
  const box = (await icon.boundingBox())!
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2)
  await page.mouse.down()
  await page.waitForTimeout(600)
  await page.mouse.up()
}

test.beforeEach(async ({ page }) => {
  await mockBackend(page)
  await page.goto('/')
  await expect(list(page).getByText('alpha bash session')).toBeVisible()
  await expect(list(page).getByText('beta node session')).toBeVisible()
})

test('a tap expands the search bar, focuses the input, and blur collapses it', async ({ page }) => {
  const icon = page.getByRole('button', { name: SEARCH_ICON })
  const input = page.getByRole('textbox', { name: SEARCH_INPUT })

  // Collapsed by default: one icon, no search box.
  await expect(icon).toBeVisible()
  await expect(input).toHaveCount(0)

  await icon.click()
  await expect(input).toBeVisible()
  // The whole point of the tap path: type straight away.
  await expect(input).toBeFocused()

  await input.fill('bash')
  await expect(list(page).getByText('alpha bash session')).toBeVisible()
  await expect(list(page).getByText('beta node session')).toHaveCount(0)

  // Moving focus off the bar collapses it back to the icon; the search itself
  // stays active (the icon carries the active dot).
  await input.press('Tab')
  await expect(input).toHaveCount(0)
  await expect(icon).toBeVisible()

  // Re-opening shows the still-active search, ready to edit.
  await icon.click()
  await expect(input).toBeVisible()
  await expect(input).toBeFocused()
  await expect(input).toHaveValue('bash')

  // The X clears the text but keeps the bar open for a new query…
  await page.getByRole('button', { name: 'Clear search' }).click()
  await expect(input).toBeVisible()
  await expect(input).toBeFocused()
  await expect(input).toHaveValue('')
  await expect(list(page).getByText('beta node session')).toBeVisible()

  // …and closes it once the text is gone.
  await page.getByRole('button', { name: 'Close search' }).click()
  await expect(input).toHaveCount(0)
})

test('a hold opens the quick menu instead, and its follow-up click is swallowed', async ({
  page,
}) => {
  const icon = page.getByRole('button', { name: SEARCH_ICON })
  const input = page.getByRole('textbox', { name: SEARCH_INPUT })
  const statusTrigger = page.getByRole('combobox', { name: 'Filter by status' })

  await holdSearchIcon(page)

  // The menu is up…
  await expect(statusTrigger).toBeVisible()
  // …but the search bar did not expand, neither from the hold nor from the
  // click the browser synthesizes when the press ends.
  await expect(input).toHaveCount(0)
  await page.waitForTimeout(100)
  await expect(input).toHaveCount(0)

  // A tap on the icon now just dismisses the menu; it must not expand the
  // search bar either.
  await icon.click()
  await expect(statusTrigger).toBeHidden()
  await expect(input).toHaveCount(0)
})

test('the quick menu changes filters and clears them again', async ({ page }) => {
  const input = page.getByRole('textbox', { name: SEARCH_INPUT })

  await holdSearchIcon(page)
  await page.getByRole('combobox', { name: 'Filter by status' }).click()
  await page.getByRole('option', { name: 'Running' }).click()

  // The pick applied (the stopped card is gone) and the menu stayed open for
  // the next change.
  await expect(list(page).getByText('beta node session')).toHaveCount(0)
  await expect(page.getByRole('combobox', { name: 'Filter by status' })).toHaveText(/Running/)

  await page.getByRole('button', { name: 'Clear filters' }).click()
  await expect(list(page).getByText('beta node session')).toBeVisible()
  await expect(page.getByRole('combobox', { name: 'Filter by status' })).toHaveText(/All status/)

  // Taps outside the menu dismiss it; the search bar still never opened.
  // (A raw coordinate below the cards: the list's empty area, which is no
  // session card and no part of the header.)
  await page.mouse.click(187, 700)
  await expect(page.getByRole('combobox', { name: 'Filter by status' })).toBeHidden()
  await expect(input).toHaveCount(0)

  // Escape closes it too.
  await holdSearchIcon(page)
  await expect(page.getByRole('combobox', { name: 'Filter by status' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.getByRole('combobox', { name: 'Filter by status' })).toBeHidden()
  await expect(input).toHaveCount(0)
})
