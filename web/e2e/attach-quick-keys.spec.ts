import { test, expect, type Page } from '@playwright/test'

/**
 * Attach-mode coverage for the mobile quick keys.
 *
 * The reported regression: tapping or holding the arrow keys let xterm keep DOM
 * focus, so the soft keyboard stayed up over the panel and its viewport churn
 * cancelled the pointer mid-hold. These tests pin the invariant that a key
 * press parks focus on the pressed button, and that one press sends exactly one
 * input frame (no double send from the pointer/key path plus the click).
 */

const SESSION_ID = 'attach-session'

function summary() {
  return {
    id: SESSION_ID,
    title: 'Attach test',
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

/** Stubs everything but the socket, and records every input frame the client sends. */
async function stubApi(page: Page, inputs: string[]): Promise<void> {
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
        if (parsed.type === 'input' && parsed.data !== undefined) inputs.push(parsed.data)
      } catch {
        // Non-JSON or unrelated frame: not an input.
      }
    })
    ws.send(initFrame('$ '))
  })
}

/** The active element's tag plus a stable description of what it is. */
async function focusState(page: Page) {
  return page.evaluate(() => {
    const active = document.activeElement
    if (!active) return 'none'
    const label = active.getAttribute('aria-label')
    const id = active.getAttribute('id')
    const cls = active.className?.toString?.() ?? ''
    return `${active.tagName.toLowerCase()}${label ? `[${label}]` : ''}${id ? `#${id}` : ''}${
      cls.includes('xterm')
        ? ` xterm(${cls
            .split(/\s+/)
            .filter((c) => c.includes('xterm'))
            .join('.')})`
        : ''
    }`
  })
}

test('a quick-key press parks focus on the key, not on xterm', async ({ page }) => {
  const inputs: string[] = []
  await stubApi(page, inputs)

  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(`/session/${SESSION_ID}?mode=attach`)
  const left = page.getByRole('button', { name: 'Left' })
  await expect(left).toBeVisible()
  await expect(page.locator('#main-container').getByText('Attached')).toBeVisible()

  // Precondition: xterm holds DOM focus (that is the state the regression
  // failed to change), so a press that leaves focus alone keeps the keyboard.
  await page.locator('.xterm').click({ position: { x: 40, y: 40 } })
  expect(await focusState(page)).toContain('textarea')

  await left.click()
  expect(await focusState(page)).toBe('button[Left]')

  // One press, one input frame: the pointer path and the click path must not
  // both fire.
  await expect.poll(() => inputs.length).toBe(1)
  expect(inputs).toEqual(['\x1b[D'])
})

test('holding a quick key repeats and keeps focus', async ({ page }) => {
  const inputs: string[] = []
  await stubApi(page, inputs)

  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(`/session/${SESSION_ID}?mode=attach`)
  const left = page.getByRole('button', { name: 'Left' })
  await expect(left).toBeVisible()
  await expect(page.locator('#main-container').getByText('Attached')).toBeVisible()

  await page.locator('.xterm').click({ position: { x: 40, y: 40 } })
  const box = await left.boundingBox()
  expect(box).not.toBeNull()
  await page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2)
  await page.mouse.down()
  await page.waitForTimeout(700)
  await page.mouse.up()

  // Immediate send, one after the 400ms delay, then repeats every 100ms.
  expect(inputs.length).toBeGreaterThanOrEqual(4)
  expect(inputs.every((data) => data === '\x1b[D')).toBe(true)
  expect(await focusState(page)).toBe('button[Left]')
})
