import { test, expect, type Page } from '@playwright/test'

/**
 * The detail page asks for three things: the summary, the logs, and a live
 * events stream. The stream has to open, or the header sits in "Reconnecting"
 * and re-fetches the summary — which would re-render the header mid-assertion.
 */
async function seedSessionDetail(
  page: Page,
  sessionId: string,
  summary: Record<string, unknown> = {}
): Promise<void> {
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
  await page.route(`**/api/sessions/${sessionId}`, (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        id: sessionId,
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
        ...summary,
      }),
    })
  )
  await page.route(`**/api/sessions/${sessionId}/logs**`, (route) => {
    if (new URL(route.request().url()).pathname.endsWith('/tail')) {
      return route.fulfill({ status: 200, body: '', headers: { 'x-log-resizes': '[]' } })
    }
    return route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ offset: 0, chunks: [], total: 0, resizes: [] }),
    })
  })
}

test('session detail mobile actions have thumb-sized targets and remain usable on short screens', async ({
  page,
}) => {
  await page.setViewportSize({ width: 375, height: 667 })
  await seedSessionDetail(page, 'mobile-session')
  let removeRequests = 0
  await page.route('**/api/sessions/mobile-session?**', (route) => {
    removeRequests += 1
    expect(route.request().method()).toBe('DELETE')
    expect(new URL(route.request().url()).searchParams.get('force')).toBe('true')
    return route.fulfill({ status: 200, contentType: 'application/json', body: '{"removed":true}' })
  })

  await page.goto('/session/mobile-session?mode=logs')
  const trigger = page.getByRole('button', { name: 'Session actions' })
  await expect(trigger).toBeVisible()
  const triggerBox = await trigger.boundingBox()
  expect(triggerBox?.width).toBeGreaterThanOrEqual(44)
  expect(triggerBox?.height).toBeGreaterThanOrEqual(44)

  await trigger.click()
  const items = page.getByRole('menuitem')
  await expect(items).toHaveCount(6)
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
  await trigger.click()
  await page.getByRole('menuitem', { name: 'Delete' }).click()
  const confirm = page.getByRole('alertdialog')
  await expect(confirm).toContainText('The running process will be killed first.')
  await confirm.getByRole('button', { name: 'Cancel' }).click()
  expect(removeRequests).toBe(0)

  await page.setViewportSize({ width: 1024, height: 768 })
  await expect(trigger).toBeHidden()
  await expect(page.getByRole('button', { name: 'Refresh' })).toBeVisible()
  await page.getByRole('button', { name: 'Delete', exact: true }).click()
  await confirm.getByRole('button', { name: 'Delete session' }).click()
  await expect.poll(() => removeRequests).toBe(1)
  await expect(page).toHaveURL('/')
})

test('the mobile header scrolls its metadata instead of squeezing the session id', async ({
  page,
}) => {
  // A full session id is wider than a phone, and it was the only item in the
  // header willing to shrink: it collapsed to its first character while every
  // badge kept its size. The row scrolls now, and the actions button stays out
  // of the scrolling part so it cannot be pushed off the right edge.
  const sessionId = 'b7f2c9d4-8a1e-4c3b-9f2d-5e6a7b8c9d0e'
  await page.setViewportSize({ width: 375, height: 667 })
  // Running with an agent report is the busiest the header gets: the id, the
  // status badge and the program badge, all in one row.
  await seedSessionDetail(page, sessionId, {
    program_status: { state: 'working', app: 'claude', progress: 42 },
  })
  await page.goto(`/session/${sessionId}?mode=logs`)

  const idButton = page.getByRole('button', { name: sessionId })
  const strip = page.getByTestId('session-header-meta')
  const trigger = page.getByRole('button', { name: 'Session actions' })

  // The id is measured where the row has room to spare and where it does not:
  // equal widths mean nothing squeezed it.
  const idWidthAt = async (width: number) => {
    await page.setViewportSize({ width, height: 667 })
    return (await idButton.boundingBox())?.width ?? 0
  }
  const onPhone = await idWidthAt(375)
  await expect(idButton).toBeInViewport()
  const withRoom = await idWidthAt(1280)
  expect(onPhone).toBeGreaterThan(withRoom - 1)

  // Wider content than the strip has room for is what "the badges scroll"
  // means: had they shrunk to fit, there would be no overflow to scroll.
  await page.setViewportSize({ width: 375, height: 667 })
  const overflow = await strip.evaluate((el) => el.scrollWidth - el.clientWidth)
  expect(overflow).toBeGreaterThan(0)

  // The button is the rightmost thing on the row, on screen and outside the
  // scrolling part.
  const stripBox = await strip.boundingBox()
  const triggerBox = await trigger.boundingBox()
  expect(stripBox).not.toBeNull()
  expect(triggerBox).not.toBeNull()
  expect(triggerBox!.x).toBeGreaterThanOrEqual(stripBox!.x + stripBox!.width - 1)
  expect(triggerBox!.x + triggerBox!.width).toBeLessThanOrEqual(375)

  // The badge the phone could not fit is one swipe away, and the swipe leaves
  // the button where it was.
  await strip.evaluate((el) => el.scrollTo({ left: el.scrollWidth }))
  await expect(page.getByText('Working 42%')).toBeInViewport()
  expect(await trigger.boundingBox()).toEqual(triggerBox)
})
