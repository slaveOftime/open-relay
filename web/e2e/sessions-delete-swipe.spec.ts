import { test, expect, type Page } from '@playwright/test'

async function swipe(
  page: Page,
  selector: string,
  fromX: number,
  toX: number,
  fromY = 240,
  toY = 247,
  inspectMove?: () => Promise<void>
) {
  await page.evaluate(
    ({ selector, fromX, toX, fromY, toY }) => {
      const target = document.querySelector(selector)
      if (!target) throw new Error(`Missing swipe target: ${selector}`)
      const start = new Touch({ identifier: 1, target, clientX: fromX, clientY: fromY })
      const moved = new Touch({ identifier: 1, target, clientX: toX, clientY: toY })
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
        new TouchEvent('touchmove', {
          bubbles: true,
          cancelable: true,
          touches: [moved],
          targetTouches: [moved],
          changedTouches: [moved],
        })
      )
    },
    { selector, fromX, toX, fromY, toY }
  )
  await inspectMove?.()
  await page.evaluate(
    ({ selector, toX, toY }) => {
      const target = document.querySelector(selector)
      if (!target) throw new Error(`Missing swipe target: ${selector}`)
      const end = new Touch({ identifier: 1, target, clientX: toX, clientY: toY })
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
    { selector, toX, toY }
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
  let listRequests = 0
  let holdNextList = false
  let failNextList = false
  let resumeRefresh: (() => void) | undefined
  await page.route('**/api/sessions?**', async (route) => {
    listRequests += 1
    if (holdNextList) {
      holdNextList = false
      await new Promise<void>((resolve) => {
        resumeRefresh = resolve
      })
    }
    if (failNextList) {
      failNextList = false
      return route.fulfill({
        status: 503,
        contentType: 'application/json',
        body: '{"error":"network unavailable"}',
      })
    }
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
  const content = list.getByTestId('mobile-session-content')
  const swipeX = () =>
    content.evaluate((el) => new DOMMatrixReadOnly(getComputedStyle(el).transform).m41)
  await expect(list.getByText('Local session')).toBeVisible()
  const indicator = list.getByTestId('mobile-pull-indicator')
  const pullY = () =>
    content.evaluate((el) => new DOMMatrixReadOnly(getComputedStyle(el).transform).m42)
  const beforeRefresh = listRequests
  await swipe(page, '[data-testid="mobile-session-list"]', 170, 170, 140, 190, async () => {
    await expect.poll(pullY).toBeGreaterThan(20)
    await expect(indicator).toContainText('Pull to refresh')
  })
  await expect.poll(pullY).toBeCloseTo(0, 0)
  expect(listRequests).toBe(beforeRefresh)

  holdNextList = true
  await swipe(page, '[data-testid="mobile-session-list"]', 170, 170, 140, 280, async () => {
    await expect.poll(pullY).toBeGreaterThan(50)
    await expect(indicator).toContainText('Release to refresh')
    await expect(page).toHaveURL('/') // refresh only starts on release
  })
  await expect.poll(() => listRequests).toBe(beforeRefresh + 1)
  await expect(indicator).toContainText('Refreshing…')
  await expect(indicator).toHaveAttribute('aria-hidden', 'false')
  await expect(indicator.locator('svg')).toHaveClass(/animate-spin/)
  await expect.poll(pullY).toBeGreaterThan(50)
  await swipe(page, '[data-testid="mobile-session-list"]', 250, 110, 240, 247)
  await expect(page).toHaveURL('/')
  await expect(indicator).toContainText('Refreshing…') // a second gesture cannot interrupt it
  resumeRefresh?.()
  await expect.poll(pullY).toBeCloseTo(0, 0)
  await expect(indicator).toHaveAttribute('aria-hidden', 'true')
  await swipe(page, '[data-node-swipe-ignore]', 170, 170, 140, 280, async () => {
    expect(await pullY()).toBe(0)
  })
  expect(listRequests).toBe(beforeRefresh + 1)

  const scrolled = await page.evaluate(() => {
    const listElement = document.querySelector('[data-testid="mobile-session-list"]') as HTMLElement
    const spacer = document.createElement('div')
    spacer.dataset.pullTestSpacer = 'true'
    spacer.style.height = '1000px'
    listElement.appendChild(spacer)
    const documentScroller = document.scrollingElement as HTMLElement
    const scroller =
      documentScroller.scrollHeight > documentScroller.clientHeight ? documentScroller : listElement
    if (scroller === listElement) {
      listElement.style.height = '300px'
      listElement.style.overflowY = 'auto'
    }
    scroller.scrollTop = 160
    return scroller.scrollTop
  })
  expect(scrolled).toBeGreaterThan(0)
  await swipe(page, '[data-testid="mobile-session-list"]', 170, 170, 140, 280, async () => {
    expect(await pullY()).toBe(0)
    await expect(indicator).toHaveAttribute('aria-hidden', 'true')
  })
  expect(listRequests).toBe(beforeRefresh + 1)
  await page.evaluate(() => {
    document.scrollingElement!.scrollTop = 0
    const listElement = document.querySelector('[data-testid="mobile-session-list"]') as HTMLElement
    listElement.scrollTop = 0
    listElement.style.height = ''
    listElement.style.overflowY = ''
    document.querySelector('[data-pull-test-spacer]')?.remove()
  })

  await page.emulateMedia({ reducedMotion: 'reduce' })
  holdNextList = true
  await swipe(page, '[data-testid="mobile-session-list"]', 170, 170, 140, 280, async () => {
    await page.waitForTimeout(50)
    expect(await pullY()).toBe(0)
    await expect(indicator).toContainText('Release to refresh')
    await expect(indicator).toHaveCSS('opacity', '1')
  })
  await expect.poll(() => listRequests).toBe(beforeRefresh + 2)
  await expect(indicator).toContainText('Refreshing…')
  await expect(indicator.locator('svg')).not.toHaveClass(/animate-spin/)
  resumeRefresh?.()
  await expect(indicator).toHaveAttribute('aria-hidden', 'true')
  await page.emulateMedia({ reducedMotion: 'no-preference' })

  failNextList = true
  const beforeFailure = listRequests
  await swipe(page, '[data-testid="mobile-session-list"]', 170, 170, 140, 280)
  await expect.poll(() => listRequests).toBe(beforeFailure + 1)
  const errorDialog = page.getByRole('dialog')
  await expect(errorDialog).toContainText('network unavailable')
  await expect(list.getByText('Local session')).toBeVisible()
  await errorDialog.getByRole('button', { name: 'Close' }).first().click()
  await expect(indicator).toHaveAttribute('aria-hidden', 'true')

  await swipe(page, '[data-testid="mobile-session-list"]', 250, 215, 240, 247, async () => {
    await expect.poll(swipeX).toBeLessThan(-8)
  })
  await expect(page).toHaveURL('/') // short drags give feedback but do not navigate
  await expect.poll(swipeX).toBeCloseTo(0, 0)
  await swipe(page, '[data-testid="mobile-session-list"]', 250, 110, 240, 247, async () => {
    await expect.poll(swipeX).toBeLessThan(-15)
    await expect(page).toHaveURL('/') // preview begins before switching nodes
  })
  await expect.poll(swipeX).toBeCloseTo(0, 0)
  await expect(page).toHaveURL(/node=worker-a/)
  await expect(list.getByText('worker-a session')).toBeVisible()
  await swipe(page, '[data-node-swipe-ignore]', 250, 110, 240, 247, async () => {
    expect(await swipeX()).toBe(0)
  })
  await expect(page).toHaveURL(/node=worker-a/)
  await swipe(page, '[data-testid="mobile-session-list"]', 250, 180, 110, 300, async () => {
    expect(await swipeX()).toBe(0)
  })
  await expect(page).toHaveURL(/node=worker-a/)
  await swipe(page, '[data-testid="mobile-session-list"]', 250, 110)
  await expect(page).toHaveURL(/node=worker-b/)
  await swipe(page, '[data-testid="mobile-session-list"]', 110, 250)
  await expect(page).toHaveURL(/node=worker-a/)

  await page.emulateMedia({ reducedMotion: 'reduce' })
  await swipe(page, '[data-testid="mobile-session-list"]', 250, 110, 240, 247, async () => {
    await page.waitForTimeout(50)
    expect(await swipeX()).toBe(0)
  })
  await expect(page).toHaveURL(/node=worker-b/)
  await page.emulateMedia({ reducedMotion: 'no-preference' })
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
