import { test, expect, type Page } from '@playwright/test'

/**
 * The sessions list's group headers: pressing one collapses its group, and the
 * header sticks to the top of the list while the group's cards/rows scroll
 * under it.
 *
 * The file runs at a phone viewport by construction — the tap and the card
 * list are the mobile path, and one case covers the desktop table. The
 * mobile-Safari project runs the same cases, where a tap and `position:
 * sticky` are the behaviours most likely to differ.
 */
test.use({ viewport: { width: 375, height: 812 } })

const PREFS_KEY = 'open-relay.webv2.sessions.preferences.v1'

/** Seed the grouping the page loads with, the way the page stores it. */
async function seedGrouping(page: Page, groupBy: 'command' | 'tag') {
  await page.addInitScript(
    ([key, value]) => {
      window.localStorage.setItem(key, JSON.stringify({ groupBy: value }))
    },
    [PREFS_KEY, groupBy]
  )
}

function makeSession(
  id: string,
  title: string,
  command: string,
  { tags = [], ageSeconds = 60 }: { tags?: string[]; ageSeconds?: number } = {}
) {
  return {
    id,
    title,
    tags,
    command,
    args: [],
    pid: 1234,
    status: 'stopped',
    created_at: new Date(Date.now() - ageSeconds * 1000).toISOString(),
    cwd: '/tmp',
    input_needed: false,
    node: null,
    last_total_bytes: 0,
  }
}

async function mockSessions(page: Page, items: ReturnType<typeof makeSession>[]) {
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
  await page.route('**/api/sessions/events**', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'text/event-stream',
      body: `event: stream_ready\ndata: ${JSON.stringify({ version: 2, nodes: [] })}\n\n`,
    })
  )
  await page.route('**/api/sessions**', (route) => {
    if (new URL(route.request().url()).pathname === '/api/sessions/events') {
      void route.fallback()
      return
    }
    void route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ items, total: items.length, offset: 0, limit: 15 }),
    })
  })
}

/** The mobile card list's content — the desktop table holds the same titles. */
const list = (page: Page) => page.getByTestId('mobile-session-content')

test('tapping a group header hides its group, and tapping again brings it back', async ({
  page,
}) => {
  await seedGrouping(page, 'command')
  await mockSessions(page, [
    makeSession('s-bash-1', 'alpha bash session', 'bash', { ageSeconds: 10 }),
    makeSession('s-bash-2', 'beta bash session', 'bash', { ageSeconds: 20 }),
    makeSession('s-node-1', 'gamma node session', 'node', { ageSeconds: 30 }),
  ])
  await page.goto('/')

  const bashHeader = list(page).getByRole('button', { name: 'Bash', exact: true })
  const nodeHeader = list(page).getByRole('button', { name: 'Node.js', exact: true })
  await expect(list(page).getByText('alpha bash session')).toBeVisible()
  await expect(bashHeader).toHaveAttribute('aria-expanded', 'true')

  await bashHeader.click()

  // The group's sessions leave the DOM; the header stays, because it is the
  // only way back. Other groups are untouched.
  await expect(list(page).getByText('alpha bash session')).toHaveCount(0)
  await expect(list(page).getByText('beta bash session')).toHaveCount(0)
  await expect(bashHeader).toHaveAttribute('aria-expanded', 'false')
  await expect(bashHeader).toBeVisible()
  await expect(nodeHeader).toHaveAttribute('aria-expanded', 'true')
  await expect(list(page).getByText('gamma node session')).toBeVisible()

  await bashHeader.click()
  await expect(list(page).getByText('alpha bash session')).toBeVisible()
  await expect(list(page).getByText('beta bash session')).toBeVisible()
  await expect(bashHeader).toHaveAttribute('aria-expanded', 'true')
})

test('the header sticks to the list top while its group scrolls under it', async ({ page }) => {
  const bash = Array.from({ length: 12 }, (_, i) =>
    makeSession(`s-bash-${i}`, `bash session ${String(i).padStart(2, '0')}`, 'bash', {
      ageSeconds: i * 10,
    })
  )
  await seedGrouping(page, 'command')
  await mockSessions(page, bash)
  await page.goto('/')

  const scroller = page.getByTestId('mobile-session-list')
  const header = list(page).getByRole('button', { name: 'Bash', exact: true })
  await expect(list(page).getByText('bash session 00')).toBeVisible()

  // At rest the header sits on the scrollport's top edge…
  const scrollerBox = (await scroller.boundingBox())!
  const before = (await header.boundingBox())!
  expect(Math.abs(before.y - scrollerBox.y)).toBeLessThanOrEqual(1)

  // …and a scroll of the list itself (not the window) must not take it away.
  const scrolled = await scroller.evaluate((el) => {
    el.scrollTop = 320
    return el.scrollTop
  })
  expect(scrolled).toBeGreaterThanOrEqual(320)

  const after = (await header.boundingBox())!
  expect(Math.abs(after.y - scrollerBox.y)).toBeLessThanOrEqual(1)
  await expect(header).toBeVisible()

  // The header is not merely positioned at the top: it paints above the cards
  // sliding under it, whose content carries z-indexes of its own.
  const onTop = await header.evaluate((el) => {
    const rect = el.getBoundingClientRect()
    // The table can be wider than the viewport; keep the probe point on screen.
    const x = Math.min(rect.left + rect.width - 8, window.innerWidth - 8)
    const hit = document.elementFromPoint(x, rect.top + rect.height / 2)
    return hit !== null && (hit === el || el.contains(hit))
  })
  expect(onTop).toBe(true)

  // It keeps holding through a deeper scroll, too.
  await scroller.evaluate((el) => {
    el.scrollTop = 640
  })
  const deeper = (await header.boundingBox())!
  expect(Math.abs(deeper.y - scrollerBox.y)).toBeLessThanOrEqual(1)
})

test('collapsing a command group does not collapse a same-named tag group', async ({ page }) => {
  await seedGrouping(page, 'command')
  await mockSessions(page, [
    makeSession('s-bash', 'alpha session', 'bash', { tags: ['bash'], ageSeconds: 10 }),
    makeSession('s-node', 'beta session', 'node', { ageSeconds: 20 }),
  ])
  await page.goto('/')

  await list(page).getByRole('button', { name: 'Bash', exact: true }).click()
  await expect(list(page).getByText('alpha session')).toHaveCount(0)

  // Switch the grouping from the quick menu. The tag "bash" is a different
  // group from the command "Bash": the collapse must not follow the label.
  await page.getByRole('button', { name: 'Search sessions' }).click()
  await page.getByRole('combobox', { name: 'Group sessions by' }).click()
  await page.getByRole('option', { name: 'Tag' }).click()
  await page.keyboard.press('Escape')

  // Tag labels are the raw tag names, not the command display names.
  const tagHeader = list(page).getByRole('button', { name: 'bash', exact: true })
  await expect(tagHeader).toHaveAttribute('aria-expanded', 'true')
  await expect(list(page).getByText('alpha session')).toBeVisible()

  // The tag group collapses on its own, as the command group did.
  await tagHeader.click()
  await expect(list(page).getByText('alpha session')).toHaveCount(0)
  await expect(tagHeader).toHaveAttribute('aria-expanded', 'false')
})

test('the desktop table header collapses its rows and stays at the table top', async ({ page }) => {
  const bash = Array.from({ length: 30 }, (_, i) =>
    makeSession(`s-bash-${i}`, `bash session ${String(i).padStart(2, '0')}`, 'bash', {
      ageSeconds: i * 10,
    })
  )
  await seedGrouping(page, 'command')
  await mockSessions(page, [
    ...bash,
    makeSession('s-node', 'standalone node session', 'node', { ageSeconds: 10_000 }),
  ])
  await page.setViewportSize({ width: 1280, height: 800 })
  await page.goto('/')

  const table = page.getByRole('table')
  const bashHeader = table.getByRole('button', { name: 'Bash', exact: true })
  await expect(table.getByText('bash session 00')).toBeVisible()

  // Toggle: rows leave the DOM, the header stays.
  await bashHeader.click()
  await expect(table.getByText('bash session 00')).toHaveCount(0)
  await expect(bashHeader).toHaveAttribute('aria-expanded', 'false')
  await expect(table.getByText('standalone node session')).toBeVisible()

  await bashHeader.click()
  await expect(table.getByText('bash session 00')).toBeVisible()

  // Sticky: scroll the table's own scrollport. Walking up to the element that
  // actually overflows finds the wrapper without pinning its class list.
  const scrolled = await table.evaluate((el, top) => {
    let node = el.parentElement as HTMLElement | null
    while (node && node.scrollHeight <= node.clientHeight + 1) node = node.parentElement
    if (!node) return null
    node.scrollTop = top
    return { scrollTop: node.scrollTop, top: node.getBoundingClientRect().top }
  }, 300)
  expect(scrolled).not.toBeNull()
  expect(scrolled!.scrollTop).toBeGreaterThanOrEqual(300)

  const headerBox = (await bashHeader.boundingBox())!
  expect(Math.abs(headerBox.y - scrolled!.top)).toBeLessThanOrEqual(1)
  await expect(bashHeader).toBeVisible()

  // And it paints above the rows that slide under it.
  const onTop = await bashHeader.evaluate((el) => {
    const rect = el.getBoundingClientRect()
    // The table can be wider than the viewport; keep the probe point on screen.
    const x = Math.min(rect.left + rect.width - 8, window.innerWidth - 8)
    const hit = document.elementFromPoint(x, rect.top + rect.height / 2)
    return hit !== null && (hit === el || el.contains(hit))
  })
  expect(onTop).toBe(true)
})
