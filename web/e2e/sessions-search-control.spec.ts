import { test, expect, type Page } from '@playwright/test'

/**
 * The mobile header's search control. The control is mobile-only, so the file
 * runs at a phone viewport in the desktop Chromium project — pointer events
 * make the tap/double-tap/drag gestures identical to touch there.
 */
test.use({ viewport: { width: 375, height: 812 } })

// A real touch tap is a different code path from a synthetic mouse click: no
// mousedown focus, and a click that arrives after the press already focused the
// search input. The suite's mouse-driven tests cannot see this, so the gestures
// run under touch input here.
test.describe('with touch input', () => {
  test.use({ hasTouch: true })

  test('a tap expands the bar and hands focus to the input', async ({ page }) => {
    const icon = page.getByRole('button', { name: SEARCH_ICON })
    const input = page.getByRole('textbox', { name: SEARCH_INPUT })

    await icon.tap()

    await expect(input).toBeVisible()
    // The bar must keep the input focused: the tap's own click moves focus to
    // the trigger as its default action, which would blur the input and close
    // the bar it just opened.
    await expect(input).toBeFocused()
    await page.waitForTimeout(400)
    await expect(input).toBeVisible()
    await expect(input).toBeFocused()
  })

  test('a double tap opens the quick menu', async ({ page }) => {
    const icon = page.getByRole('button', { name: SEARCH_ICON })
    const input = page.getByRole('textbox', { name: SEARCH_INPUT })
    const statusTrigger = page.getByRole('combobox', { name: 'Filter by status' })

    const box = (await icon.boundingBox())!
    const cx = box.x + box.width / 2
    const cy = box.y + box.height / 2

    // The pair must land inside the 300ms double window, and two awaited CDP
    // round trips can drift past it under load — a miss classifies as two
    // singles, which toggle the bar twice and settle back closed. So tap
    // again until the pair lands; every attempt starts from the same state.
    for (let attempt = 0; attempt < 4; attempt++) {
      await page.touchscreen.tap(cx, cy)
      await page.touchscreen.tap(cx, cy)
      await page.waitForTimeout(400) // let a missed pair's two singles fire
      if (await statusTrigger.isVisible()) break
    }
    await expect(statusTrigger).toBeVisible()
    // The menu replaced the bar the first tap of the pair opened.
    await expect(input).toHaveCount(0)
  })
})

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

test('the trigger floats at the scroll-handle height, right-aligned', async ({ page }) => {
  const icon = page.getByRole('button', { name: SEARCH_ICON })
  const box = (await icon.boundingBox())!

  // Right side of the screen, in the thumb zone.
  expect(box.x + box.width / 2).toBeGreaterThan(375 / 2)
  // The terminal scroll handle's offset (`bottom-70`): raised well clear of the
  // bottom meta bar, without floating up into the header.
  expect(box.y).toBeGreaterThan(375)
  expect(box.y + box.height).toBeLessThan(650)
  // It is a layer over the list, not list content, so scrolling cannot take it
  // away.
  expect(await icon.evaluate((el) => el.closest('[data-testid="mobile-session-list"]'))).toBeNull()
})

/** Double-tap the search icon: the quick menu's opener. */
async function openQuickMenu(page: Page) {
  await page.getByRole('button', { name: SEARCH_ICON }).dblclick()
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

  // Collapsed by default: one floating trigger, no search box.
  await expect(icon).toBeVisible()
  await expect(input).toHaveCount(0)

  await icon.click()
  await expect(input).toBeVisible()
  // The whole point of the tap path: type straight away.
  await expect(input).toBeFocused()

  // The bar grows out of the trigger, to the left: the trigger itself does not
  // move, and the input sits on its row as one merged pill.
  const collapsed = (await icon.boundingBox())!
  await input.fill('x')
  const expandedTrigger = (await icon.boundingBox())!
  const inputBox = (await input.boundingBox())!
  expect(expandedTrigger.x).toBe(collapsed.x)
  expect(inputBox.x).toBeLessThan(collapsed.x)
  expect(inputBox.y).toBeLessThan(collapsed.y + collapsed.height)
  expect(inputBox.y + inputBox.height).toBeGreaterThan(collapsed.y)

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

test('a double tap opens the quick menu, and its follow-up clicks are swallowed', async ({
  page,
}) => {
  const icon = page.getByRole('button', { name: SEARCH_ICON })
  const input = page.getByRole('textbox', { name: SEARCH_INPUT })
  const statusTrigger = page.getByRole('combobox', { name: 'Filter by status' })

  await openQuickMenu(page)

  // The menu is up…
  await expect(statusTrigger).toBeVisible()

  // …anchored to the trigger's left, centred on it, with a small gap.
  const menuBox = (await page.getByTestId('session-search-menu').boundingBox())!
  const iconBox = (await icon.boundingBox())!
  const gap = iconBox.x - (menuBox.x + menuBox.width)
  expect(menuBox.x + menuBox.width).toBeLessThan(iconBox.x)
  expect(gap).toBeLessThan(24)
  // Vertically centred on the trigger (the menu is taller than it).
  expect(Math.abs(menuBox.y + menuBox.height / 2 - (iconBox.y + iconBox.height / 2))).toBeLessThan(
    8
  )

  // …and no search bar is left behind: the menu replaced the bar the first tap
  // of the pair opened, and the clicks the browser synthesizes for the pair
  // open nothing further.
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

  await openQuickMenu(page)
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
  // (A raw coordinate in the list's empty area, left of the bottom-right
  // trigger cluster and below the cards.)
  await page.mouse.click(60, 400)
  await expect(page.getByRole('combobox', { name: 'Filter by status' })).toBeHidden()
  await expect(input).toHaveCount(0)

  // Escape closes it too.
  await openQuickMenu(page)
  await expect(page.getByRole('combobox', { name: 'Filter by status' })).toBeVisible()
  await page.keyboard.press('Escape')
  await expect(page.getByRole('combobox', { name: 'Filter by status' })).toBeHidden()
  await expect(input).toHaveCount(0)
})

test('a drag relocates the trigger and remembers the spot', async ({ page }) => {
  const icon = page.getByRole('button', { name: SEARCH_ICON })
  const input = page.getByRole('textbox', { name: SEARCH_INPUT })
  const statusTrigger = page.getByRole('combobox', { name: 'Filter by status' })

  const before = (await icon.boundingBox())!
  const cx = before.x + before.width / 2
  const cy = before.y + before.height / 2

  // No hold, no arm cue: on this button a travelling press is a move. The
  // menu — which the old hold used to open mid-press, fighting the drag —
  // must stay shut the whole way.
  await page.mouse.move(cx, cy)
  await page.mouse.down()
  await page.mouse.move(cx - 60, cy + 80, { steps: 6 })
  await expect(statusTrigger).toHaveCount(0)
  await page.mouse.up()

  // The trigger tracked the pointer exactly — down-left of `right-5 bottom-70`
  // always has room, so no clamp could have adjusted it.
  const after = (await icon.boundingBox())!
  expect(Math.abs(after.x - (before.x - 60))).toBeLessThanOrEqual(2)
  expect(Math.abs(after.y - (before.y + 80))).toBeLessThanOrEqual(2)

  // Taking the button means the press was never a tap: the release's
  // follow-up click opens no bar.
  await expect(input).toHaveCount(0)

  // The new spot survives a reload…
  await page.reload()
  await expect(list(page).getByText('alpha bash session')).toBeVisible()
  const restored = (await icon.boundingBox())!
  expect(Math.abs(restored.x - after.x)).toBeLessThanOrEqual(2)
  expect(Math.abs(restored.y - after.y)).toBeLessThanOrEqual(2)

  // …and the relocated trigger still works.
  await icon.click()
  await expect(input).toBeVisible()
  await expect(input).toBeFocused()

  // Expanding grows the bar toward the left, from the icon: a dragged control
  // is left-anchored, and this is exactly the case where a bar that grew with
  // the flow pushed the icon right instead. Wait for the browser's actual
  // width transition, rather than duplicating the component's duration in a
  // fixed sleep (which can still sample mid-animation on a busy worker).
  await input.evaluate(async (field) => {
    const bar = field.parentElement!.parentElement!
    await Promise.all(bar.getAnimations().map((animation) => animation.finished))
  })
  await expect
    .poll(async () => {
      const expanded = (await icon.boundingBox())!
      return Math.abs(expanded.x - restored.x)
    })
    .toBeLessThanOrEqual(2)
  await expect
    .poll(async () => {
      const inputBox = (await input.boundingBox())!
      return inputBox.x + inputBox.width
    })
    .toBeLessThanOrEqual(restored.x + 1)
})
