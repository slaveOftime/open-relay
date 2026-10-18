import { test, expect, type Page } from '@playwright/test'

/**
 * The desktop collapse for the attach panel.
 *
 * The toggle lives in the session info bar (sm+ only) and folds the panel to
 * zero width: the terminal reclaims the space and no rail is left behind.
 * Below sm the panel is a drawer and the key bar's toggle is the only control,
 * so this spec pins both halves. It runs on the desktop Chromium project and
 * the iPhone WebKit profile, so each test sets the viewport it needs instead of
 * trusting the project default.
 */

const SESSION_ID = 'panel-toggle'

function summary() {
  return {
    id: SESSION_ID,
    title: 'Panel toggle',
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

/** The rendered terminal width — it tracks the space the panel leaves. */
async function terminalWidth(page: Page): Promise<number> {
  return page.evaluate(() => {
    const screen = document.querySelector('.xterm-screen')
    return screen ? Math.round(screen.getBoundingClientRect().width) : 0
  })
}

/** The right edge of the painted terminal grid. */
async function terminalRight(page: Page): Promise<number> {
  return page.evaluate(() => {
    const screen = document.querySelector('.xterm-screen')
    return screen ? Math.round(screen.getBoundingClientRect().right) : 0
  })
}

test('the info bar toggle hides the panel and the terminal takes the space back', async ({
  page,
}) => {
  await stageAttach(page)
  await page.setViewportSize({ width: 1280, height: 800 })
  await page.goto(`/session/${SESSION_ID}?mode=attach`)

  const inputLabel = page.getByText('Text Input', { exact: true })
  await expect(inputLabel).toBeVisible()
  const openWidth = await terminalWidth(page)
  expect(openWidth).toBeGreaterThan(0)

  const hide = page.getByRole('button', { name: 'Hide input panel' })
  await expect(hide).toHaveAttribute('aria-expanded', 'true')
  await hide.click()

  await expect(inputLabel).toBeHidden()
  const show = page.getByRole('button', { name: 'Show input panel' })
  await expect(show).toHaveAttribute('aria-expanded', 'false')
  await expect.poll(() => terminalWidth(page)).toBeGreaterThan(openWidth + 100)
  const collapsedWidth = await terminalWidth(page)

  // Nothing of the panel is left behind: the grid now ends at the body's edge,
  // so a leftover rail would push this back out.
  await expect.poll(() => terminalRight(page)).toBeGreaterThan(page.viewportSize()!.width - 40)

  await show.click()

  await expect(inputLabel).toBeVisible()
  await expect.poll(() => terminalWidth(page)).toBeLessThan(collapsedWidth - 100)
})

test('the info bar toggle stays out of the mobile layout', async ({ page }) => {
  await stageAttach(page)
  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(`/session/${SESSION_ID}?mode=attach`)

  // Below sm the panel is a drawer; the key bar's toggle is the control.
  await expect(page.getByRole('button', { name: 'Hide input panel' })).toBeHidden()
  await expect(page.getByRole('button', { name: 'Open input panel' })).toBeVisible()
})
