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

/** Mode bits in the INIT header; see `WS_FLAG_*` in `src/api/ws-frames.ts`. */
const WS_FLAG_MOUSE_REPORT = 1 << 2
const WS_FLAG_SGR_MOUSE = 1 << 3

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

/**
 * INIT frame: `[tag=1][flags:u8][endOffset:u64][incarnation:u64][running:u8][attachmentId:u64][data]`.
 */
function initFrame(data: string, flags = 0): Buffer {
  const header = Buffer.alloc(27)
  header[0] = 1
  header[1] = flags
  header.writeBigUInt64BE(0n, 2)
  header.writeBigUInt64BE(0n, 10)
  header[18] = 1
  header.writeBigUInt64BE(1n, 19)
  return Buffer.concat([header, Buffer.from(data)])
}

/**
 * Stubs everything but the socket, and records every input frame the client
 * sends. `init` overrides what the attached "program" announces on connect:
 * the header's mode bits (mirroring `WS_FLAG_*` in `src/api/ws-frames.ts`) are
 * what actually turn mouse reporting on — the client re-syncs xterm to them
 * after writing the seed data.
 */
async function stubApi(
  page: Page,
  inputs: string[],
  init: { data?: string; flags?: number } = {}
): Promise<void> {
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
    ws.send(initFrame(init.data ?? '$ ', init.flags ?? 0))
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

/** The ESC byte, named so it stays out of pattern literals (see below). */
const ESC = '\x1b'

/**
 * SGR mouse reports for the wheel buttons (64 = up, 65 = down) as `[col, row]`.
 * With `?1000` + `?1006` on, xterm reports every wheel at the cell under its
 * coordinates — which is how the emitter's point becomes observable here.
 *
 * ESC is interpolated rather than written into the pattern so that
 * `no-control-regex` keeps flagging accidental control characters elsewhere.
 */
const WHEEL_REPORT = new RegExp(`${ESC}\\[<(6[45]);(\\d+);(\\d+)M`, 'g')

function wheelReports(inputs: string[]): Array<{ col: number; row: number }> {
  const reports: Array<{ col: number; row: number }> = []
  for (const frame of inputs) {
    for (const match of frame.matchAll(WHEEL_REPORT)) {
      reports.push({ col: Number(match[2]), row: Number(match[3]) })
    }
  }
  return reports
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

test('the key composer hint explains the rules for touch and keyboard', async ({ page }) => {
  const inputs: string[] = []
  await stubApi(page, inputs)

  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(`/session/${SESSION_ID}?mode=attach`)
  await expect(page.locator('#main-container').getByText('Attached')).toBeVisible()

  // The composer is in the drawer, collapsed on the phone viewport.
  await page.getByRole('button', { name: 'Open input panel' }).click()
  const hint = page.getByRole('button', { name: 'Key input rules' })
  await expect(hint).toBeVisible()

  // Focus, not hover: Radix ignores touch pointer-moves, so focus is the path
  // that behaves identically on Chromium and the mobile-Safari profile.
  await hint.focus()
  const tooltip = page.getByRole('tooltip')
  await expect(tooltip).toBeVisible()
  await expect(tooltip).toContainText('each extra space sends a Space')
  await expect(tooltip).toContainText('Named keys')
})

test('a hold-then-drag moves the scroll handle and remembers it', async ({ page }) => {
  const inputs: string[] = []
  await stubApi(page, inputs)

  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(`/session/${SESSION_ID}?mode=attach`)
  const handle = page.getByRole('button', { name: 'Scroll terminal' })
  await expect(handle).toBeVisible()
  await expect(page.locator('#main-container').getByText('Attached')).toBeVisible()

  const before = (await handle.boundingBox())!
  const cx = before.x + before.width / 2
  const cy = before.y + before.height / 2

  // Hold still: the arm ring marks the threshold where the next move carries
  // the handle instead of scrolling with it. The assertion is the sync.
  await page.mouse.move(cx, cy)
  await page.mouse.down()
  await expect(handle).toHaveClass(/ring-2/)
  await page.mouse.move(cx - 70, cy + 80, { steps: 6 })
  // The joystick stood down — the press belongs to the move now — so the handle
  // is back at its resting opacity rather than showing the scroll cue.
  await expect(handle).toHaveClass(/opacity-70/)
  await page.mouse.up()

  // Exact tracking (no clamp: `right-5 bottom-70` leaves room down-left), and
  // the move counted as neither a tap (the ring did not toggle) nor a still
  // held press (the arm cue cleared on release).
  const after = (await handle.boundingBox())!
  expect(Math.abs(after.x - (before.x - 70))).toBeLessThanOrEqual(2)
  expect(Math.abs(after.y - (before.y + 80))).toBeLessThanOrEqual(2)
  await expect(handle).toHaveAttribute('aria-expanded', 'false')
  await expect(handle).not.toHaveClass(/ring-2/)

  // A press that moves immediately is still the joystick gesture — scrolling,
  // not repositioning, and not a tap on the ring either.
  const held = (await handle.boundingBox())!
  await page.mouse.move(held.x + 24, held.y + 24)
  await page.mouse.down()
  await page.mouse.move(held.x + 30, held.y + 100, { steps: 4 })
  await expect(handle).toHaveClass(/opacity-90/)
  await page.mouse.up()
  const unmoved = (await handle.boundingBox())!
  expect(unmoved.x).toBe(held.x)
  expect(unmoved.y).toBe(held.y)
  await expect(handle).toHaveAttribute('aria-expanded', 'false')

  // The moved spot is remembered across a reload, and the handle still works.
  await page.reload()
  await expect(handle).toBeVisible()
  await expect(page.locator('#main-container').getByText('Attached')).toBeVisible()
  const restored = (await handle.boundingBox())!
  expect(Math.abs(restored.x - after.x)).toBeLessThanOrEqual(2)
  expect(Math.abs(restored.y - after.y)).toBeLessThanOrEqual(2)

  await handle.click()
  await expect(handle).toHaveAttribute('aria-expanded', 'true')
  await expect(page.getByRole('button', { name: /^Send / }).first()).toBeVisible()
})

test('a synthetic wheel reports the cell under the scroll handle', async ({ page }) => {
  const inputs: string[] = []
  // Mouse tracking on: the attached "program" sees every wheel as a report of
  // the cell under its coordinates, which makes the emitter's point observable.
  await stubApi(page, inputs, { flags: WS_FLAG_MOUSE_REPORT | WS_FLAG_SGR_MOUSE })

  await page.setViewportSize({ width: 390, height: 844 })
  await page.goto(`/session/${SESSION_ID}?mode=attach`)
  const handle = page.getByRole('button', { name: 'Scroll terminal' })
  await expect(handle).toBeVisible()
  await expect(page.locator('#main-container').getByText('Attached')).toBeVisible()

  // Dispatch a wheel on the terminal itself at `point` (its centre when null)
  // and return the report it produces — the ground truth for that location.
  const wheelAt = async (point: { x: number; y: number } | null) => {
    const before = wheelReports(inputs).length
    await page.evaluate((at) => {
      const target =
        document.querySelector('.xterm-scrollable-element') ??
        document.querySelector('.xterm-screen') ??
        document.querySelector('.xterm')
      if (!target) throw new Error('terminal not mounted')
      const rect = target.getBoundingClientRect()
      const where = at ?? { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 }
      target.dispatchEvent(
        new WheelEvent('wheel', {
          deltaY: -3,
          deltaMode: 1, // DOM_DELTA_LINE: whole lines, no sub-line rounding
          clientX: where.x,
          clientY: where.y,
          bubbles: true,
          cancelable: true,
        })
      )
    }, point)
    await expect.poll(() => wheelReports(inputs).length).toBeGreaterThan(before)
    return wheelReports(inputs)[before]
  }

  // The handle's centre, read in-page: exactly the rect the emitter reads.
  const point = await handle.evaluate((element) => {
    const rect = element.getBoundingClientRect()
    return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 }
  })

  // Ground truth: a wheel at the handle's centre names its cell, and the same
  // wheel at the terminal's centre names a different one — so the checks below
  // really do distinguish where the wheel came from.
  const expected = await wheelAt(point)
  const centre = await wheelAt(null)
  expect(centre).not.toEqual(expected)

  // The wheel forwarded from the handle lands on the same cell: the emitter
  // maps the handle's position instead of defaulting to the target's centre.
  const beforeForward = wheelReports(inputs).length
  await page.evaluate((at) => {
    document.querySelector('[aria-label="Scroll terminal"]')?.dispatchEvent(
      new WheelEvent('wheel', {
        deltaY: -3,
        deltaMode: 1,
        clientX: at.x,
        clientY: at.y,
        bubbles: true,
        cancelable: true,
      })
    )
  }, point)
  await expect.poll(() => wheelReports(inputs).length).toBeGreaterThan(beforeForward)
  expect(wheelReports(inputs)[beforeForward]).toEqual(expected)

  // The joystick loop emits the same mapped wheels while it runs: the handle
  // does not move under the drag, so every report stays on that same cell.
  const beforeDrag = wheelReports(inputs).length
  await page.mouse.move(point.x, point.y)
  await page.mouse.down()
  await page.mouse.move(point.x, point.y + 100, { steps: 4 })
  await expect.poll(() => wheelReports(inputs).length).toBeGreaterThan(beforeDrag)
  await page.mouse.up()
  const dragged = wheelReports(inputs).slice(beforeDrag)
  expect(dragged.length).toBeGreaterThan(0)
  expect(
    dragged.every((report) => report.col === expected.col && report.row === expected.row)
  ).toBe(true)
})
