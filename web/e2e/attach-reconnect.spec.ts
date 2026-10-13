import { test, expect, type Page } from '@playwright/test'

/**
 * Attach-socket reconnection coverage.
 *
 * This is the behaviour that a rewrite of the socket effect can break without
 * any unit test noticing: *when* the client opens a new WebSocket after a drop.
 * The policy is: an unexpected close reconnects with a backoff, an explicit
 * session-end frame does not, and a stale "already connecting" attempt does not
 * double-connect.
 *
 * It runs on both Chromium and mobile Safari (see playwright.config.ts), because
 * the whole reconnect path exists for iOS PWA lifecycle events, where the
 * socket is dropped rather than closed cleanly.
 */

const SESSION_ID = 'reconnect-session'

function sessionSummary() {
  return {
    id: SESSION_ID,
    title: 'Reconnect test',
    tags: [],
    command: 'bash',
    args: [],
    pid: 4321,
    status: 'running',
    created_at: new Date().toISOString(),
    started_at: null,
    ended_at: null,
    cwd: '/tmp',
    input_needed: false,
    notifications_enabled: false,
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
  /** Close the current socket the way a dropped network connection would. */
  dropConnection: () => void
  /** Send an explicit session-end frame. */
  sendEnded: () => void
}

/**
 * Stubs the REST/SSE surface and hands control of the attach socket to the
 * caller. Only one socket is alive at a time, so `dropConnection` always affects
 * the most recent one — which is what a real drop does.
 */
async function openAttach(page: Page): Promise<AttachHarness> {
  const harness: AttachHarness = {
    connections: 0,
    dropConnection: () => latest.close(),
    sendEnded: () => latest.send(endedFrame()),
  }
  // The most recently opened socket; reassigned on every reconnect.
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
      body: '{"auth_required":false}',
    })
  )
  await page.route('**/api/nodes', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '[]' })
  )
  await page.route('**/api/push/public-key', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '{"public_key":null}' })
  )
  await page.route('**/api/push/subscriptions', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '{"ok":true}' })
  )
  await page.route(`**/api/sessions/${SESSION_ID}`, (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(sessionSummary()),
    })
  )

  await page.routeWebSocket(/\/api\/sessions\/[^/]+\/attach/, (ws) => {
    // A new connection whenever the client retries.
    harness.connections += 1
    latest = ws
    ws.send(initFrame('$ '))
  })

  return harness
}

async function gotoAttach(page: Page) {
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(`/session/${SESSION_ID}?mode=attach`)
  await expect(page.locator('#main-container').getByText('Attached')).toBeVisible()
}

test('an unexpected socket drop opens a new attach connection', async ({ page }) => {
  const ws = await openAttach(page)
  await gotoAttach(page)
  expect(ws.connections).toBe(1)

  ws.dropConnection()

  // The client backs off by 1s and throttles repeats, so a reconnect inside
  // 6s means it reconnected; this is the assertion a silent policy regression
  // would fail.
  await expect.poll(() => ws.connections, { timeout: 6000, intervals: [100] }).toBeGreaterThan(1)
})

test('an explicit session-end frame closes the socket for good', async ({ page }) => {
  const ws = await openAttach(page)
  await gotoAttach(page)
  expect(ws.connections).toBe(1)

  ws.sendEnded()

  // A finished session must not be reattached, even though the socket closed
  // exactly the way a dropped connection does. Give it well past the backoff and
  // the 1200ms force-throttle before concluding.
  await page.waitForTimeout(4000)
  expect(ws.connections).toBe(1)
  // An end frame is terminal: the page switches to the log view rather than
  // quietly sitting on a dead terminal.
  await expect(page).toHaveURL(/mode=logs/)
  await expect(page.getByRole('spinbutton', { name: 'Tail line limit' })).toBeVisible()
})

test('a repeated drop keeps reconnecting rather than giving up', async ({ page }) => {
  const ws = await openAttach(page)
  await gotoAttach(page)

  for (let drop = 0; drop < 3; drop += 1) {
    ws.dropConnection()
    await expect.poll(() => ws.connections, { timeout: 6000, intervals: [100] }).toBe(drop + 2)
  }
})

test('a reconnect reads Reconnecting, not Connecting', async ({ page }) => {
  const ws = await openAttach(page)
  await gotoAttach(page)

  ws.dropConnection()

  // Once the socket has opened at least once, a drop is a *re*connect. This is
  // the only thing `wsEverConnected` decides, and it is the easiest state to
  // drop when the socket handlers are reworked.
  await expect(page.locator('#main-container')).toContainText('Reconnecting')
  await expect.poll(() => ws.connections, { timeout: 6000, intervals: [100] }).toBeGreaterThan(1)
})
