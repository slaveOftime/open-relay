import { devices, expect, test, type Page } from '@playwright/test'

/**
 * Android soft-keyboard viewport coverage for attach mode.
 *
 * The reported regression: after tapping the terminal, the on-screen keyboard
 * covers the bottom rows; with the cursor parked at the bottom, the user cannot
 * see what they type. The page is a full-height flex layout, so there is no
 * scroll range to reveal the terminal. Shift the body just far enough to bring
 * the cursor above the IME instead: resizing the terminal would make FitAddon
 * report a new grid and send a PTY resize every time focus opens or closes the
 * keyboard, and shifting by the full overlap would push a middle/top cursor
 * out of the top of the screen.
 *
 * Playwright cannot raise a real IME, so the test replaces the visual
 * viewport's height (the one signal the browser gives the page) and dispatches
 * the same `resize` event a keyboard would. Everything else is the real
 * terminal, ResizeObserver, FitAddon and attach socket.
 */

const SESSION_ID = 'soft-keyboard-viewport'
const KEYBOARD_HEIGHT = 320
/** A row near the middle of the terminal, with plenty of output below it. */
const ABOVE_KEYBOARD_CURSOR_ROW = 20

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

/** DATA frame: `[tag=2][offset:u64][data]`. */
function dataFrame(offset: number, data: string): Buffer {
  const header = Buffer.alloc(9)
  header[0] = 2
  header.writeBigUInt64BE(BigInt(offset), 1)
  return Buffer.concat([header, Buffer.from(data)])
}

interface ViewportHarness {
  inputs: string[]
  messages: Array<Record<string, unknown>>
  /** Send more terminal output through the mocked attach socket. */
  sendData?: (data: string) => void
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

async function stubApi(page: Page, harness: ViewportHarness, output: string): Promise<void> {
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
    // The init frame's snapshot ends at offset 0; later DATA frames start
    // there and advance by the bytes they carry.
    let offset = 0
    harness.sendData = (data: string) => {
      ws.send(dataFrame(offset, data))
      offset += Buffer.byteLength(data)
    }
    // Enough content that the cursor starts on the last rendered row.
    ws.send(initFrame(output))
  })
}

async function openAttach(
  page: Page,
  harness: ViewportHarness,
  output = `${'line\r\n'.repeat(80)}$ `
): Promise<void> {
  await installVisualViewportProbe(page)
  await stubApi(page, harness, output)
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

/** The painted cursor cell, in viewport coordinates. */
function cursorRect(page: Page): Promise<{ top: number; bottom: number } | null> {
  return page.evaluate(() => {
    const cursor = document.querySelector('.xterm-rows .xterm-cursor')
    if (!cursor) return null
    const rect = cursor.getBoundingClientRect()
    return { top: rect.top, bottom: rect.bottom }
  })
}

/** The body transform the keyboard shift currently applies, if any. */
function mainTransform(page: Page): Promise<string> {
  return page.evaluate(() => {
    const main = document.querySelector('#main-container')
    return main ? getComputedStyle(main).transform : ''
  })
}

/** Text on the last rendered row — where the cursor sits in this fixture. */
function lastVisibleRow(page: Page): Promise<string> {
  return page.evaluate(() => {
    const rows = document.querySelectorAll('.xterm-rows > div')
    return rows.length > 0 ? (rows[rows.length - 1].textContent ?? '') : ''
  })
}

/** Let the initial FitAddon layout settle. */
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

function resizeCount(harness: ViewportHarness): number {
  return harness.messages.filter((message) => message.type === 'resize').length
}

test('the keyboard shifts only enough to reveal a bottom cursor without resizing it', async ({
  page,
}) => {
  const harness: ViewportHarness = { inputs: [], messages: [] }
  await openAttach(page, harness)

  const layoutHeight = await page.evaluate(() => document.documentElement.clientHeight)
  const fullRows = await settleRows(page)
  expect(fullRows).toBeGreaterThan(0)
  // Let the initial fit and its debounced resize report settle.
  await page.waitForTimeout(300)
  const resizesBeforeKeyboard = resizeCount(harness)
  // The fixture ends in a shell prompt; that row is the cursor's row.
  expect(await lastVisibleRow(page)).toContain('$')
  const bottomBeforeKeyboard = await terminalBottom(page)

  // The terminal starts at the bottom of the page, underneath where the
  // keyboard will be. This is the setup for the report.
  expect(bottomBeforeKeyboard).toBeGreaterThan(layoutHeight - KEYBOARD_HEIGHT)

  await setKeyboardHeight(page, KEYBOARD_HEIGHT)

  const keyboardTop = layoutHeight - KEYBOARD_HEIGHT
  // The cursor row must clear the keyboard; the body shift is what moves it.
  await expect.poll(() => terminalBottom(page)).toBeLessThanOrEqual(keyboardTop + 1)
  const cursor = await cursorRect(page)
  expect(cursor).not.toBeNull()
  expect(cursor!.bottom).toBeLessThanOrEqual(keyboardTop + 1)
  // ...and just above it: the page moves by the cursor's shortfall, not the
  // whole keyboard height.
  expect(cursor!.bottom).toBeGreaterThan(keyboardTop - 32)
  // The shift is driven by the cursor, not the full keyboard inset.
  expect(await mainTransform(page)).not.toBe('none')
  // The body shifts, not the terminal: same grid, same PTY size.
  expect(await terminalRows(page)).toBe(fullRows)
  expect(resizeCount(harness)).toBe(resizesBeforeKeyboard)
  // The row being typed on is still rendered.
  expect(await lastVisibleRow(page)).toContain('$')

  // Typing still reaches the session, and focus is not lost to the shift.
  await expect(page.locator('.xterm-helper-textarea')).toBeFocused()
  await page.keyboard.type('hi')
  await expect.poll(() => harness.inputs.join('')).toBe('hi')

  // Dismissing the keyboard restores the layout without a resize either.
  await setKeyboardHeight(page, 0)
  await expect.poll(() => terminalBottom(page)).toBeGreaterThan(layoutHeight - KEYBOARD_HEIGHT)
  await page.waitForTimeout(300)
  expect(await terminalRows(page)).toBe(fullRows)
  expect(resizeCount(harness)).toBe(resizesBeforeKeyboard)
  expect(await lastVisibleRow(page)).toContain('$')
})

test('the keyboard leaves the view alone when the cursor is already above it', async ({ page }) => {
  const harness: ViewportHarness = { inputs: [], messages: [] }
  await openAttach(page, harness, `${'line\r\n'.repeat(80)}\x1b[${ABOVE_KEYBOARD_CURSOR_ROW};1H$ `)

  const layoutHeight = await page.evaluate(() => document.documentElement.clientHeight)
  const fullRows = await settleRows(page)
  expect(fullRows).toBeGreaterThan(0)
  await page.waitForTimeout(300)
  const resizesBeforeKeyboard = resizeCount(harness)
  const keyboardTop = layoutHeight - KEYBOARD_HEIGHT
  const bottomBeforeKeyboard = await terminalBottom(page)
  const cursorBeforeKeyboard = await cursorRect(page)
  expect(cursorBeforeKeyboard).not.toBeNull()
  expect(cursorBeforeKeyboard!.bottom).toBeLessThan(keyboardTop)

  await setKeyboardHeight(page, KEYBOARD_HEIGHT)
  // Give the inset subscription and its layout effect time to run.
  await page.waitForTimeout(200)

  // The cursor was already visible above the IME, so nothing may move. The
  // keyboard is allowed to cover the bottom of the terminal.
  expect(await mainTransform(page)).toBe('none')
  expect(await terminalBottom(page)).toBe(bottomBeforeKeyboard)
  expect(await terminalBottom(page)).toBeGreaterThan(keyboardTop)
  const cursorAfterKeyboard = await cursorRect(page)
  expect(cursorAfterKeyboard).not.toBeNull()
  expect(cursorAfterKeyboard!.bottom).toBeLessThanOrEqual(keyboardTop)

  // The terminal grid and PTY size are still untouched.
  expect(await terminalRows(page)).toBe(fullRows)
  expect(resizeCount(harness)).toBe(resizesBeforeKeyboard)

  // Output can move the cursor after the keyboard is already open; the shift
  // must follow it so the line being typed never disappears under the IME.
  harness.sendData?.(`\x1b[${fullRows};1H$ `)
  await expect.poll(() => mainTransform(page)).not.toBe('none')
  const cursorAtBottom = await cursorRect(page)
  expect(cursorAtBottom).not.toBeNull()
  expect(cursorAtBottom!.bottom).toBeLessThanOrEqual(keyboardTop + 1)
  expect(await terminalRows(page)).toBe(fullRows)
  expect(resizeCount(harness)).toBe(resizesBeforeKeyboard)

  // The keyboard is up and the terminal still receives input.
  await expect(page.locator('.xterm-helper-textarea')).toBeFocused()
  await page.keyboard.type('hi')
  await expect.poll(() => harness.inputs.join('')).toBe('hi')

  // Closing it also leaves the view where it was.
  await setKeyboardHeight(page, 0)
  await expect.poll(() => mainTransform(page)).toBe('none')
  await page.waitForTimeout(200)
  expect(await terminalBottom(page)).toBe(bottomBeforeKeyboard)
  expect(await terminalRows(page)).toBe(fullRows)
  expect(resizeCount(harness)).toBe(resizesBeforeKeyboard)
})
