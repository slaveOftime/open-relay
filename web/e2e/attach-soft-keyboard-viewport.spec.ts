import { devices, expect, test, type Page } from '@playwright/test'

/**
 * Android soft-keyboard viewport coverage for attach mode.
 *
 * The reported regression: after tapping the terminal, the on-screen keyboard
 * covers the bottom rows; with the cursor parked at the bottom, the user cannot
 * see what they type. The page is a full-height flex layout, so there is no
 * scroll range to reveal the terminal — the only fix is to fit the shell to
 * `window.visualViewport`, let FitAddon drop rows and report the smaller grid.
 *
 * Playwright cannot raise a real IME, so the test replaces the visual
 * viewport's height (the one signal the browser gives the page) and dispatches
 * the same `resize` event a keyboard would. Everything else is the real
 * terminal, ResizeObserver, FitAddon and attach socket.
 */

const SESSION_ID = 'soft-keyboard-viewport'
const KEYBOARD_HEIGHT = 320

/** A Pixel 7 reports Android and has touch, so the mobile layout is active. */
test.use({ ...devices['Pixel 7'] })

function summary() {
  return {
    id: SESSION_ID,
    title: 'Keyboard viewport test',
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
  header.writeBigUInt64BE(0n, 2)
  header.writeBigUInt64BE(0n, 10)
  header[18] = 1
  header.writeBigUInt64BE(1n, 19)
  return Buffer.concat([header, Buffer.from(data)])
}

interface ViewportHarness {
  inputs: string[]
  messages: Array<Record<string, unknown>>
}

/**
 * Replaces `visualViewport.height` with a controllable value. The real
 * `VisualViewport` object is kept, so focus, scroll and the resize listeners
 * the page installed behave exactly as they do in the browser.
 */
async function installVisualViewportProbe(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const viewport = window.visualViewport
    if (!viewport) return
    let height = viewport.height
    Object.defineProperty(viewport, 'height', {
      configurable: true,
      get: () => height,
    })
    ;(
      window as Window & { __setSoftKeyboardHeight?: (keyboardHeight: number) => void }
    ).__setSoftKeyboardHeight = (keyboardHeight: number) => {
      const layoutHeight = document.documentElement.clientHeight || window.innerHeight
      height = Math.max(1, layoutHeight - keyboardHeight)
      viewport.dispatchEvent(new Event('resize'))
    }
  })
}

async function setKeyboardHeight(page: Page, keyboardHeight: number): Promise<void> {
  await page.evaluate((height) => {
    ;(
      window as Window & { __setSoftKeyboardHeight?: (keyboardHeight: number) => void }
    ).__setSoftKeyboardHeight?.(height)
  }, keyboardHeight)
}

async function stubApi(page: Page, harness: ViewportHarness): Promise<void> {
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
    route.fulfill({ status: 200, contentType: 'application/json', body: '{"auth_required":false}' })
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
    route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(summary()) })
  )

  await page.routeWebSocket(/\/api\/sessions\/[^/]+\/attach/, (ws) => {
    ws.onMessage((message) => {
      if (typeof message !== 'string') return
      try {
        const parsed = JSON.parse(message) as { type?: string; data?: string }
        if (parsed.type === 'input' && parsed.data !== undefined) {
          harness.inputs.push(parsed.data)
        } else {
          harness.messages.push(parsed as Record<string, unknown>)
        }
      } catch {
        // Non-JSON or unrelated frame.
      }
    })
    // Enough content that the cursor starts on the last rendered row.
    ws.send(initFrame(`${'line\r\n'.repeat(80)}$ `))
  })
}

async function openAttach(page: Page, harness: ViewportHarness): Promise<void> {
  await installVisualViewportProbe(page)
  await stubApi(page, harness)
  await page.goto(`/session/${SESSION_ID}?mode=attach`)
  await expect(page.locator('#main-container').getByText('Attached')).toBeVisible()
  await page.locator('.xterm').click({ position: { x: 40, y: 40 } })
  await expect(page.locator('.xterm-helper-textarea')).toBeFocused()
}

/** Rows currently rendered by xterm — the grid the PTY is drawing for. */
function terminalRows(page: Page): Promise<number> {
  return page.evaluate(() => document.querySelectorAll('.xterm-rows > div').length)
}

/** The bottom edge of the terminal surface, in viewport coordinates. */
function terminalBottom(page: Page): Promise<number> {
  return page.evaluate(() => document.querySelector('.xterm')?.getBoundingClientRect().bottom ?? -1)
}

/** Text on the last rendered row — where the cursor sits in this fixture. */
function lastVisibleRow(page: Page): Promise<string> {
  return page.evaluate(() => {
    const rows = document.querySelectorAll('.xterm-rows > div')
    return rows.length > 0 ? (rows[rows.length - 1].textContent ?? '') : ''
  })
}

/** Let the ResizeObserver -> FitAddon -> debounced resize report settle. */
async function settleRows(page: Page): Promise<number> {
  let previous = await terminalRows(page)
  for (let attempt = 0; attempt < 40; attempt += 1) {
    await page.waitForTimeout(50)
    const next = await terminalRows(page)
    if (next > 0 && next === previous) return next
    previous = next
  }
  return previous
}

function lastResize(harness: ViewportHarness): { rows: number; cols: number } | null {
  return (
    (harness.messages.filter((message) => message.type === 'resize').at(-1) as
      { rows: number; cols: number } | undefined) ?? null
  )
}

test('the keyboard viewport keeps the terminal cursor above the IME', async ({ page }) => {
  const harness: ViewportHarness = { inputs: [], messages: [] }
  await openAttach(page, harness)

  const layoutHeight = await page.evaluate(() => document.documentElement.clientHeight)
  const fullRows = await settleRows(page)
  expect(fullRows).toBeGreaterThan(0)
  // The fixture ends in a shell prompt; that row is the cursor's row.
  expect(await lastVisibleRow(page)).toContain('$')

  // The terminal starts at the bottom of the page, underneath where the
  // keyboard will be. This is the setup for the report.
  expect(await terminalBottom(page)).toBeGreaterThan(layoutHeight - KEYBOARD_HEIGHT)

  await setKeyboardHeight(page, KEYBOARD_HEIGHT)

  const shrunkRows = await settleRows(page)
  const keyboardTop = layoutHeight - KEYBOARD_HEIGHT
  // Nothing may be left under the keyboard — least of all the cursor row.
  await expect.poll(() => terminalBottom(page)).toBeLessThanOrEqual(keyboardTop + 1)
  expect(shrunkRows).toBeLessThan(fullRows)
  // The row being typed on is still rendered — the refit may not leave the
  // viewport parked on older scrollback.
  expect(await lastVisibleRow(page)).toContain('$')

  // The PTY has to learn the new grid, or programs keep drawing for rows the
  // user cannot see.
  await expect
    .poll(() => lastResize(harness)?.rows ?? -1, {
      message: 'the PTY must be resized to the grid above the keyboard',
    })
    .toBe(shrunkRows)

  // Typing still reaches the session, and focus is not lost to the refit.
  await expect(page.locator('.xterm-helper-textarea')).toBeFocused()
  await page.keyboard.type('hi')
  await expect.poll(() => harness.inputs.join('')).toBe('hi')

  // Dismissing the keyboard gives the rows back.
  await setKeyboardHeight(page, 0)
  await expect.poll(() => terminalBottom(page)).toBeGreaterThan(layoutHeight - KEYBOARD_HEIGHT)
  expect(await settleRows(page)).toBe(fullRows)
  expect(await lastVisibleRow(page)).toContain('$')
})
