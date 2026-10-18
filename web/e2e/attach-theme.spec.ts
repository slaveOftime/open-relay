import { test, expect, type Page } from '@playwright/test'

/**
 * The attached terminal follows the resolved app scheme.
 *
 * `index.css` resolves the scheme in two steps: an explicit `data-theme` on
 * <html> wins, otherwise the OS preference. The xterm palette must track both
 * sources live — a palette read once at creation (or one that only watched
 * `prefers-color-scheme`) keeps the wrong background after the OS flips or an
 * override is set, while the surrounding app has already rethemed.
 */

const SESSION_ID = 'theme-session'

/** The `background` of each palette in `xterm-theme.ts`, as CSS computed it. */
const DARK_BACKGROUND = 'rgb(3, 7, 18)'
const LIGHT_BACKGROUND = 'rgb(241, 245, 249)'

function summary() {
  return {
    id: SESSION_ID,
    title: 'Theme test',
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
  header[1] = 0
  header.writeBigUInt64BE(0n, 2)
  header.writeBigUInt64BE(0n, 10)
  header[18] = 1
  header.writeBigUInt64BE(1n, 19)
  return Buffer.concat([header, Buffer.from(data)])
}

/** Stubs the REST surface and attaches a socket that immediately sends `$ `. */
async function stageAttach(page: Page): Promise<void> {
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
    ws.send(initFrame('$ '))
  })
}

/**
 * The background xterm painted behind the grid: the theme service writes it
 * onto the scrollable element's inline style (with the initial frame, so the
 * value is the resolved theme, not a default).
 */
async function terminalBackground(page: Page): Promise<string> {
  return page.evaluate(() => {
    const element = document.querySelector('.xterm-scrollable-element')
    if (!element) return 'terminal is not mounted'
    return getComputedStyle(element).backgroundColor
  })
}

test('the terminal repaints when the OS color scheme flips', async ({ page }) => {
  await stageAttach(page)
  await page.emulateMedia({ colorScheme: 'dark' })
  await page.goto(`/session/${SESSION_ID}?mode=attach`)
  await expect
    .poll(() => terminalBackground(page), { message: 'initial theme' })
    .toBe(DARK_BACKGROUND)

  await page.emulateMedia({ colorScheme: 'light' })
  await expect
    .poll(() => terminalBackground(page), { message: 'flipped theme' })
    .toBe(LIGHT_BACKGROUND)
})

test('the terminal follows an explicit data-theme over the OS preference', async ({ page }) => {
  await stageAttach(page)
  await page.emulateMedia({ colorScheme: 'dark' })
  await page.goto(`/session/${SESSION_ID}?mode=attach`)
  await expect
    .poll(() => terminalBackground(page), { message: 'initial theme' })
    .toBe(DARK_BACKGROUND)

  await page.evaluate(() => {
    document.documentElement.dataset.theme = 'light'
  })
  await expect
    .poll(() => terminalBackground(page), { message: 'explicit light' })
    .toBe(LIGHT_BACKGROUND)

  await page.evaluate(() => {
    document.documentElement.removeAttribute('data-theme')
  })
  await expect
    .poll(() => terminalBackground(page), { message: 'back to the OS preference' })
    .toBe(DARK_BACKGROUND)
})
