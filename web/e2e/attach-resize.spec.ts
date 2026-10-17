import { test, expect, type Page } from '@playwright/test'

/**
 * Attach size reconciliation.
 *
 * The handshake URL carries the terminal's size and the daemon applies it at
 * registration. Everything the terminal recomputed between that fit and the
 * socket's `open` used to be lost: the debounced resize was a no-op while the
 * socket was CONNECTING, `afterOpen` cleared the pending size, and xterm only
 * calls back on the *next* change — so the PTY kept the stale handshake size
 * (and the program kept drawing for it) until the page was re-entered.
 *
 * Playwright opens a mocked client socket only once its route handler returns,
 * so a handler that awaits a gate holds the socket in CONNECTING for as long as
 * the test needs: resize the viewport, let the terminal refit, then open.
 */

const SESSION_ID = 'resize-session'

const PHONE_VIEWPORT = { width: 390, height: 844 }
/** Wider with room for real columns; the attach layout has no breakpoint here. */
const WIDER_VIEWPORT = { width: 600, height: 844 }

function sessionSummary() {
  return {
    id: SESSION_ID,
    title: 'Resize test',
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
 * The grid xterm is actually rendering, read from its own DOM: the char-size
 * service measures a 32-character run, the row divs are one per row.
 */
async function terminalSize(page: Page): Promise<{ cols: number; rows: number }> {
  return page.evaluate(() => {
    const screen = document.querySelector('.xterm-screen')
    const measure = [...document.querySelectorAll('.xterm-char-measure-element')].find(
      (element) => (element.textContent?.length ?? 0) === 32
    )
    const rows = document.querySelectorAll('.xterm-rows > div').length
    if (!screen || !measure) return { cols: 0, rows }
    const cellWidth = measure.getBoundingClientRect().width / 32
    const screenWidth = screen.getBoundingClientRect().width
    return { cols: cellWidth > 0 ? Math.round(screenWidth / cellWidth) : 0, rows }
  })
}

/** The layout and the webfont refit settle late; wait for the grid to stop moving. */
async function settleGrid(page: Page): Promise<{ cols: number; rows: number }> {
  let previous = await terminalSize(page)
  for (let attempt = 0; attempt < 40; attempt += 1) {
    await page.waitForTimeout(50)
    const next = await terminalSize(page)
    if (next.cols > 0 && next.cols === previous.cols && next.rows === previous.rows) {
      return next
    }
    previous = next
  }
  return previous
}

interface ResizeHarness {
  /** Query params of the attach URL: the viewport the handshake carried. */
  handshake: URLSearchParams | null
  /** Parsed client -> server messages. */
  messages: Array<Record<string, unknown>>
  /** Let the route handler return, which opens the CONNECTING client socket. */
  open: () => void
}

function resizes(harness: ResizeHarness): Array<{ rows: number; cols: number }> {
  return harness.messages.filter((message) => message.type === 'resize') as Array<{
    rows: number
    cols: number
  }>
}

/** The size the server last heard about, i.e. what the PTY is at. */
function lastReported(harness: ResizeHarness): { rows: number; cols: number } | null {
  return resizes(harness).at(-1) ?? null
}

/**
 * Wait until the size the server last heard matches the grid xterm renders.
 * The debounced report lands ~120ms after a refit, and the rendered width is
 * truncated to whole pixels, so the column count is allowed to be off by one.
 */
async function expectReportedGrid(page: Page, harness: ResizeHarness): Promise<void> {
  await expect
    .poll(
      async () => {
        const sent = lastReported(harness)
        const grid = await terminalSize(page)
        if (!sent) return Number.POSITIVE_INFINITY
        return Math.abs(sent.cols - grid.cols) + Math.abs(sent.rows - grid.rows)
      },
      { message: 'the PTY must end up at the grid the terminal renders' }
    )
    .toBeLessThanOrEqual(1)
}

/**
 * Stubs the REST surface and holds the attach socket in CONNECTING until
 * `open()` is called.
 */
async function stageAttach(page: Page): Promise<ResizeHarness> {
  let latest: WebSocketRoute
  let release: () => void = () => {}
  const gate = new Promise<void>((resolve) => {
    release = resolve
  })
  const harness: ResizeHarness = {
    handshake: null,
    messages: [],
    open: () => {
      // Returning from the handler is what opens the socket; the init frame
      // then puts the page in its attached state.
      release()
      latest.send(initFrame('$ '))
    },
  }

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

  await page.routeWebSocket(/\/api\/sessions\/[^/]+\/attach/, async (ws) => {
    latest = ws
    if (!harness.handshake) harness.handshake = new URL(ws.url()).searchParams
    ws.onMessage((message) => {
      try {
        harness.messages.push(JSON.parse(message.toString()))
      } catch {
        // Binary frames are not part of the resize contract.
      }
    })
    // Hold the socket in CONNECTING until the test calls open().
    await gate
  })

  return harness
}

test('a size the terminal took while connecting is reported when the socket opens', async ({
  page,
}) => {
  const harness = await stageAttach(page)
  await page.setViewportSize(PHONE_VIEWPORT)
  await page.goto(`/session/${SESSION_ID}?mode=attach`)
  await expect.poll(() => harness.handshake).not.toBeNull()
  const handshakeCols = Number(harness.handshake?.get('cols') ?? 0)
  expect(handshakeCols).toBeGreaterThan(0)
  const settled = await settleGrid(page)

  // The handshake is already behind us; this refit's debounced resize goes
  // nowhere because the socket is still CONNECTING.
  await page.setViewportSize(WIDER_VIEWPORT)
  await expect.poll(async () => (await terminalSize(page)).cols).toBeGreaterThan(settled.cols)
  // Let the 120ms debounce fire against the CONNECTING socket.
  await page.waitForTimeout(200)
  expect(resizes(harness)).toHaveLength(0)

  harness.open()

  await expectReportedGrid(page, harness)
  const sent = lastReported(harness)
  // The report has to be the size taken while connecting, not the handshake.
  expect(sent?.cols).toBeGreaterThan(handshakeCols)
})

test('a size the terminal takes while connected is reported right away', async ({ page }) => {
  const harness = await stageAttach(page)
  await page.setViewportSize(PHONE_VIEWPORT)
  await page.goto(`/session/${SESSION_ID}?mode=attach`)
  await expect.poll(() => harness.handshake).not.toBeNull()
  const settled = await settleGrid(page)
  harness.open()
  await expect(page.locator('#main-container').getByText('Attached')).toBeVisible()

  await page.setViewportSize(WIDER_VIEWPORT)
  await expect.poll(async () => (await terminalSize(page)).cols).toBeGreaterThan(settled.cols)

  await expectReportedGrid(page, harness)
  const sent = lastReported(harness)
  expect(sent?.cols).toBeGreaterThan(settled.cols)
})
