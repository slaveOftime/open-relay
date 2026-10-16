import { test, expect, type Page } from '@playwright/test'

/**
 * The mobile header's search control. The control is mobile-only, so the file
 * runs at a phone viewport in the desktop Chromium project — pointer events
 * make the toggling and drag gestures identical to touch there.
 */
test.use({ viewport: { width: 375, height: 812 } })

// A real touch tap is a different code path from a synthetic mouse click: the
// compatibility click lands after the browser's own press handling, and no
// mousedown focuses the button. The suite's mouse-driven tests cannot see this,
// so the gesture runs under touch input here.
test.describe('with touch input', () => {
  test.use({ hasTouch: true })

  test('a tap opens the bar and the menu, and hands focus to the input', async ({ page }) => {
    const icon = page.getByRole('button', { name: SEARCH_ICON })
    const input = page.getByRole('textbox', { name: SEARCH_INPUT })
    const statusTrigger = page.getByRole('combobox', { name: 'Filter by status' })

    await icon.tap()

    await expect(input).toBeVisible()
    // The bar must keep the input focused: the press's default would focus the
    // trigger, and the bar closes the moment its input loses focus.
    await expect(input).toBeFocused()
    // The filters came up with it: the tap opens the whole control.
    await expect(statusTrigger).toBeVisible()

    await page.waitForTimeout(400)
    await expect(input).toBeVisible()
    await expect(input).toBeFocused()
    await expect(statusTrigger).toBeVisible()
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
/** The menu's own `mr-4`: its visible right edge sits this far inside the trigger's. */
const MENU_INSET_PX = 16

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

/** One tap on the trigger: the bar and the quick menu come up together. */
async function openSearchControl(page: Page) {
  await page.getByRole('button', { name: SEARCH_ICON }).click()
}

/** Drag the trigger by a delta from wherever it currently sits. */
async function dragTrigger(page: Page, dx: number, dy: number) {
  const box = (await page.getByRole('button', { name: SEARCH_ICON }).boundingBox())!
  const cx = box.x + box.width / 2
  const cy = box.y + box.height / 2
  await page.mouse.move(cx, cy)
  await page.mouse.down()
  await page.mouse.move(cx + dx, cy + dy, { steps: 6 })
  await page.mouse.up()
}

test.beforeEach(async ({ page }) => {
  await mockBackend(page)
  await page.goto('/')
  await expect(list(page).getByText('alpha bash session')).toBeVisible()
  await expect(list(page).getByText('beta node session')).toBeVisible()
})

test('a tap opens the bar and the menu under the trigger, and a tap closes both', async ({
  page,
}) => {
  const icon = page.getByRole('button', { name: SEARCH_ICON })
  const input = page.getByRole('textbox', { name: SEARCH_INPUT })
  const statusTrigger = page.getByRole('combobox', { name: 'Filter by status' })

  // Collapsed by default: one floating trigger, no search box, no menu.
  await expect(icon).toBeVisible()
  await expect(input).toHaveCount(0)
  await expect(statusTrigger).toHaveCount(0)

  const collapsed = (await icon.boundingBox())!
  await icon.click()

  // One tap, the whole control: the bar for typing, the menu for filtering.
  await expect(input).toBeVisible()
  await expect(input).toBeFocused()
  await expect(statusTrigger).toBeVisible()

  // The bar grows out of the trigger, to the left: the trigger itself does not
  // move, and the input sits on its row as one merged pill.
  await input.fill('x')
  const expandedTrigger = (await icon.boundingBox())!
  const inputBox = (await input.boundingBox())!
  expect(expandedTrigger.x).toBe(collapsed.x)
  expect(inputBox.x).toBeLessThan(collapsed.x)
  expect(inputBox.y).toBeLessThan(collapsed.y + collapsed.height)
  expect(inputBox.y + inputBox.height).toBeGreaterThan(collapsed.y)

  // The menu hangs under the trigger's bottom-right corner: below it, right
  // edges aligned but for the menu's own inset, a small gap between them. The
  // pop's scale grows out of that corner, so both edges hold through the
  // animation.
  const menuBox = (await page.getByTestId('session-search-menu').boundingBox())!
  const iconBox = (await icon.boundingBox())!
  expect(menuBox.y).toBeGreaterThanOrEqual(iconBox.y + iconBox.height)
  expect(menuBox.y - (iconBox.y + iconBox.height)).toBeLessThanOrEqual(16)
  expect(
    Math.abs(menuBox.x + menuBox.width - (iconBox.x + iconBox.width - MENU_INSET_PX))
  ).toBeLessThanOrEqual(1)

  // Typing drives the list and leaves the menu where it is: the bar and the
  // menu are one popup, not rivals.
  await input.fill('bash')
  await expect(list(page).getByText('alpha bash session')).toBeVisible()
  await expect(list(page).getByText('beta node session')).toHaveCount(0)
  await expect(statusTrigger).toBeVisible()

  // A tap on the trigger takes both down again.
  await icon.click()
  await expect(input).toHaveCount(0)
  await expect(statusTrigger).toHaveCount(0)
})

test('a fast second tap is a second toggle, not a gesture of its own', async ({ page }) => {
  const icon = page.getByRole('button', { name: SEARCH_ICON })
  const input = page.getByRole('textbox', { name: SEARCH_INPUT })
  const statusTrigger = page.getByRole('combobox', { name: 'Filter by status' })

  await icon.dblclick()

  // The pair is two toggles: the first opened the control, the second closed
  // it. Nothing waits for a double tap, so nothing opens behind the second tap.
  await expect(input).toHaveCount(0)
  await expect(statusTrigger).toHaveCount(0)
})

test('the X clears the search text, and closes the control once it is empty', async ({ page }) => {
  const icon = page.getByRole('button', { name: SEARCH_ICON })
  const input = page.getByRole('textbox', { name: SEARCH_INPUT })
  const statusTrigger = page.getByRole('combobox', { name: 'Filter by status' })

  await openSearchControl(page)
  await input.fill('bash')
  await expect(list(page).getByText('alpha bash session')).toBeVisible()
  await expect(list(page).getByText('beta node session')).toHaveCount(0)

  // Closing and re-opening keeps the query: the control remembers it.
  await icon.click()
  await expect(input).toHaveCount(0)
  await icon.click()
  await expect(input).toHaveValue('bash')
  await expect(input).toBeFocused()

  // The X clears the text but keeps the bar open for a new query…
  await page.getByRole('button', { name: 'Clear search' }).click()
  await expect(input).toBeVisible()
  await expect(input).toBeFocused()
  await expect(input).toHaveValue('')
  await expect(list(page).getByText('beta node session')).toBeVisible()

  // …and closes the control once the text is gone.
  await page.getByRole('button', { name: 'Close search' }).click()
  await expect(input).toHaveCount(0)
  await expect(statusTrigger).toHaveCount(0)
})

test('focus staying inside the control keeps the bar up; leaving it puts it back', async ({
  page,
}) => {
  const icon = page.getByRole('button', { name: SEARCH_ICON })
  const input = page.getByRole('textbox', { name: SEARCH_INPUT })
  const statusTrigger = page.getByRole('combobox', { name: 'Filter by status' })

  await openSearchControl(page)
  await expect(input).toBeFocused()

  // Tabbing onto the bar's own X is not a dismissal: the control is one popup,
  // and its parts hand focus to each other.
  await input.press('Tab')
  await expect(input).toBeVisible()
  await expect(statusTrigger).toBeVisible()

  // Focus leaving the control — with nowhere inside to land — is.
  await input.focus()
  await input.evaluate((field) => field.blur())
  await expect(input).toHaveCount(0)
  await expect(statusTrigger).toHaveCount(0)
  await expect(icon).toBeVisible()
})

test('the quick menu changes filters and clears them again', async ({ page }) => {
  const input = page.getByRole('textbox', { name: SEARCH_INPUT })

  await openSearchControl(page)
  await page.getByRole('combobox', { name: 'Filter by status' }).click()
  await page.getByRole('option', { name: 'Running' }).click()

  // The pick applied (the stopped card is gone) and the menu stayed open for
  // the next change — with the search bar still up beside it.
  await expect(list(page).getByText('beta node session')).toHaveCount(0)
  await expect(page.getByRole('combobox', { name: 'Filter by status' })).toHaveText(/Running/)
  await expect(input).toBeVisible()

  await page.getByRole('button', { name: 'Clear filters' }).click()
  await expect(list(page).getByText('beta node session')).toBeVisible()
  await expect(page.getByRole('combobox', { name: 'Filter by status' })).toHaveText(/All status/)

  // Taps outside the control dismiss it — bar and menu together.
  // (A raw coordinate in the list's empty area, left of the bottom-right
  // trigger cluster and below the cards.)
  await page.mouse.click(60, 400)
  await expect(page.getByRole('combobox', { name: 'Filter by status' })).toBeHidden()
  await expect(input).toHaveCount(0)

  // Escape closes it too.
  await openSearchControl(page)
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
  // control — which the press's own tap would have opened — must stay shut the
  // whole way.
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

  // Taking the button means the press was never a tap: the drag swallows the
  // click that release sends, so nothing opens behind it.
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

test('a drag that never leaves the button does not toggle the control', async ({ page }) => {
  const icon = page.getByRole('button', { name: SEARCH_ICON })
  const input = page.getByRole('textbox', { name: SEARCH_INPUT })

  // A travelling press is a move, not a tap — and this one releases without ever
  // leaving the 48px button, so the browser still sends the release's click.
  // That click must not reach the control as an activation of a button the drag
  // just moved and closed.
  await dragTrigger(page, 12, 12)

  await expect(icon).toBeVisible()
  await expect(input).toHaveCount(0)
})

test('the menu opens over the trigger when there is no room under it', async ({ page }) => {
  const icon = page.getByRole('button', { name: SEARCH_ICON })
  const input = page.getByRole('textbox', { name: SEARCH_INPUT })

  // Down into the bottom band: the default spot has ~280px of room under it,
  // which this cuts to ~100 — less than the menu is tall.
  await dragTrigger(page, 0, 170)
  await icon.click()
  await expect(input).toBeVisible()

  const menuBox = (await page.getByTestId('session-search-menu').boundingBox())!
  const iconBox = (await icon.boundingBox())!
  // Flipped: the menu's bottom edge clears the trigger's top edge by the usual
  // small gap, and the whole panel is still on screen. The entrance grows out
  // of that bottom corner, so the edge holds through it.
  expect(menuBox.y + menuBox.height).toBeLessThanOrEqual(iconBox.y)
  expect(iconBox.y - (menuBox.y + menuBox.height)).toBeLessThanOrEqual(16)
  expect(menuBox.y).toBeGreaterThanOrEqual(0)
  // Right-aligned as ever: the flip is vertical only.
  expect(
    Math.abs(menuBox.x + menuBox.width - (iconBox.x + iconBox.width - MENU_INSET_PX))
  ).toBeLessThanOrEqual(1)
})

test('the menu slides into view when the trigger is dragged to the left edge', async ({ page }) => {
  const icon = page.getByRole('button', { name: SEARCH_ICON })
  const input = page.getByRole('textbox', { name: SEARCH_INPUT })

  // Past the clamp: the trigger comes to rest on the frame's left margin, where
  // a 240px panel hanging off its right edge would run off the screen.
  await dragTrigger(page, -320, 0)
  await icon.click()
  await expect(input).toBeVisible()

  const menuBox = (await page.getByTestId('session-search-menu').boundingBox())!
  const iconBox = (await icon.boundingBox())!
  expect(iconBox.x).toBeLessThan(60)
  // Still whole, and under the trigger, with its left edge slid back to the
  // margin by the placement rather than hanging off the screen.
  expect(menuBox.x + menuBox.width).toBeLessThan(375)
  expect(menuBox.y).toBeGreaterThanOrEqual(iconBox.y + iconBox.height)
  expect(menuBox.x).toBeGreaterThanOrEqual(0)
  // The entrance scales about the top-right corner, so the left edge settles
  // last — poll it rather than sampling mid-transition.
  await expect
    .poll(async () => (await page.getByTestId('session-search-menu').boundingBox())!.x)
    .toBeLessThan(8)
})
