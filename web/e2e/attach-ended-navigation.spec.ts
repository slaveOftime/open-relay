import { test, expect, type Page } from '@playwright/test'

/**
 * Navigation out of a session detail page.
 *
 * A session ending is not a user navigation: the attach view replaces its own
 * history entry when it switches to the logs view. A pushed entry would leave
 * Back pointing at the attach URL, which reconnects, ends again and switches
 * right back — the sessions list becomes unreachable.
 *
 * The header's back button is the in-app way out (a standalone PWA has no
 * browser chrome to go back with), so it is covered from the list and from a
 * deep link, where there is no history entry to pop.
 *
 * The file runs on Chromium and mobile Safari: the history and the PWA back
 * affordance are exactly the class of thing the engines differ on.
 */

const SESSION_ID = 'back-nav-session'

function sessionSummary(status: 'running' | 'stopped') {
  return {
    id: SESSION_ID,
    title: 'Back navigation',
    tags: [],
    command: 'bash',
    args: [],
    pid: 1234,
    status,
    created_at: new Date(Date.now() - 30_000).toISOString(),
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

/** INIT frame: `[tag=1][flags:u8][endOffset:u64][incarnation:u64][running:u8][attachmentId:u64][data]`. */
function initFrame(data: string): Buffer {
  const header = Buffer.alloc(27)
  header[0] = 1
  header[1] = 0
  header.writeBigUInt64BE(0n, 2)
  header.writeBigUInt64BE(0n, 10)
  header[18] = 1
  header.writeBigUInt64BE(1n, 19)
  return Buffer.concat([header, Buffer.from(data)])
}

/**
 * ENDED frame: `[tag=5][hasExitCode:u8][exitCode:i32be][finalOffset:u64be]`.
 * `finalOffset` is written as 0 so the client does not see a cursor mismatch.
 */
function endedFrame(): Buffer {
  const body = Buffer.alloc(2 + 4 + 8)
  body[0] = 5
  body[1] = 0
  body.writeInt32BE(0, 2)
  body.writeBigUInt64BE(0n, 6)
  return body
}

interface AttachHarness {
  /** Connection counter, in the order the client opened them. */
  connections: number
  /** Send the explicit end frame the way a finishing session does. */
  sendEnded: () => void
}

/**
 * Stubs the REST surface, opens an attach socket on demand, and — once
 * `sendEnded` has fired — makes every later connection end immediately, the
 * way reattaching to a finished session does. That is what turns a pushed
 * history entry into the bounce the back button must not fall into.
 */
async function openAttach(page: Page): Promise<AttachHarness> {
  const harness: AttachHarness = {
    connections: 0,
    sendEnded: () => {
      ended = true
      latest.send(endedFrame())
    },
  }
  let ended = false
  let latest: WebSocketRoute

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
  await page.route(`**/api/sessions/${SESSION_ID}`, (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(sessionSummary(ended ? 'stopped' : 'running')),
    })
  )
  await page.route(`**/api/sessions/${SESSION_ID}/logs**`, (route) => {
    if (new URL(route.request().url()).pathname.endsWith('/tail')) {
      return route.fulfill({ status: 200, body: '', headers: { 'x-log-resizes': '[]' } })
    }
    return route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ offset: 0, chunks: [], total: 0, resizes: [] }),
    })
  })
  await page.route('**/api/sessions**', (route) => {
    if (new URL(route.request().url()).pathname !== '/api/sessions') {
      return route.fallback()
    }
    const items = [sessionSummary(ended ? 'stopped' : 'running')]
    return route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ items, total: items.length, offset: 0, limit: 15 }),
    })
  })
  await page.routeWebSocket(/\/api\/sessions\/[^/]+\/attach/, (ws) => {
    harness.connections += 1
    latest = ws
    ws.send(initFrame('$ '))
    if (ended) ws.send(endedFrame())
  })

  return harness
}

/** The session card opens the attach view while its session is running. */
async function attachFromList(page: Page) {
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto('/')
  await page.getByTestId('session-card').getByRole('link', { name: 'Attach' }).click()
  await expect(page).toHaveURL(/\/session\/back-nav-session\?mode=attach/)
  await expect(page.locator('#main-container').getByText('Attached')).toBeVisible()
}

test('the header back button returns from a live attach view to the list', async ({ page }) => {
  await openAttach(page)
  await attachFromList(page)

  await expect(page.getByRole('button', { name: 'Back to sessions' })).toBeVisible()
  await page.getByRole('button', { name: 'Back to sessions' }).click()

  await expect(page).toHaveURL('/')
  await expect(page.getByTestId('session-card')).toBeVisible()
})

test('back leaves a session that ended while attached instead of looping on attach', async ({
  page,
}) => {
  const ws = await openAttach(page)
  await attachFromList(page)

  ws.sendEnded()
  await expect(page).toHaveURL(/\/session\/back-nav-session\?mode=logs/)
  await expect(page.getByRole('spinbutton', { name: 'Tail line limit' })).toBeVisible()

  // The switch to logs replaced the attach entry: one step back must reach the
  // list, not re-enter the attach URL that ends again.
  await page.goBack()

  await expect(page).toHaveURL('/')
  await expect(page.getByTestId('session-card')).toBeVisible()
})

test('a deep link into a session still reaches the list', async ({ page }) => {
  await openAttach(page)
  await page.setViewportSize({ width: 390, height: 844 })

  await page.goto(`/session/${SESSION_ID}?mode=logs`)
  await expect(page.getByRole('button', { name: 'Back to sessions' })).toBeVisible()
  await page.getByRole('button', { name: 'Back to sessions' }).click()

  await expect(page).toHaveURL('/')
  await expect(page.getByTestId('session-card')).toBeVisible()
})
