import { test, expect } from '@playwright/test'
import type { Locator, Page } from '@playwright/test'

for (const mobile of [false, true]) {
  test(`OSC 7501 status changes replace the ${mobile ? 'card' : 'row'} lifecycle label`, async ({
    page,
  }) => {
    await page.setViewportSize(mobile ? { width: 390, height: 844 } : { width: 1280, height: 800 })
    await page.addInitScript(() => {
      class Source extends EventTarget {
        onopen: ((event: Event) => void) | null = null
        onerror: ((event: Event) => void) | null = null
        constructor() {
          super()
          ;(window as Window & { emitStatus?: (data: unknown) => void }).emitStatus = (data) =>
            this.dispatchEvent(new MessageEvent('session_updated', { data: JSON.stringify(data) }))
          setTimeout(() => this.onopen?.(new Event('open')), 0)
        }
        close() {}
      }
      Object.defineProperty(window, 'EventSource', { value: Source })
    })
    const session = {
      id: 'osc7501',
      title: 'status fixture',
      tags: [],
      command: 'fixture',
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
      program_status: { state: 'working', progress: 10 },
    }
    await page.route('**/api/auth/status', (route) =>
      route.fulfill({ json: { auth_required: false } })
    )
    await page.route('**/api/nodes', (route) => route.fulfill({ json: [] }))
    await page.route('**/api/push/public-key', (route) =>
      route.fulfill({ json: { public_key: null } })
    )
    await page.route('**/api/sessions?**', (route) =>
      route.fulfill({ json: { items: [session], total: 1, offset: 0, limit: 15 } })
    )
    await page.goto('/')
    const view = mobile ? page.getByTestId('mobile-session-content') : page.getByRole('table')
    await expect(view.getByText('status fixture', { exact: true })).toBeVisible()
    await expect(view.getByText('Working 10%', { exact: true })).toBeVisible()
    await expect(view.getByText('Running', { exact: true })).toHaveCount(0)
    await expect
      .poll(() =>
        page.evaluate(() => typeof (window as Window & { emitStatus?: unknown }).emitStatus)
      )
      .toBe('function')
    for (const [report, label] of [
      [{ state: 'working', progress: 42 }, 'Working 42%'],
      [
        { state: 'blocked', kind: 'permission', msg: '<script>Approve?</script>' },
        'Permission Required',
      ],
      [{ state: 'done' }, 'Task Done'],
      [{ state: 'working', progress: 70 }, 'Working 70%'],
      [{ state: 'error' }, 'Task Error'],
      [{ state: 'idle' }, 'Idle'],
      [{ state: 'working', progress: 90 }, 'Working 90%'],
      [{ state: 'done' }, 'Task Done'],
    ] as const) {
      await page.evaluate(
        (data) => {
          ;(window as Window & { emitStatus?: (data: unknown) => void }).emitStatus?.(data)
        },
        { ...session, input_needed: report.state !== 'working', program_status: report }
      )
      await expect(view.getByText(label, { exact: true })).toBeVisible()
      // The report replaces the lifecycle badge, so no lifecycle label or
      // tooltip remains while a report is present.
      await expect(view.getByTitle('Process: Running', { exact: true })).toHaveCount(0)
      await expect(view.getByText('Running', { exact: true })).toHaveCount(0)
      await expect(view.getByText('Ready for Input', { exact: true })).toHaveCount(0)
      await expect(view.getByText('Input Needed', { exact: true })).toHaveCount(0)
    }
    await page.evaluate(
      (data) => {
        ;(window as Window & { emitStatus?: (data: unknown) => void }).emitStatus?.(data)
      },
      { ...session, program_status: null }
    )
    await expect(view.getByText('Task Done', { exact: true })).toHaveCount(0)
    await expect(view.getByText('Ready for Input', { exact: true })).toHaveCount(0)
    await expect(view.getByText('Running', { exact: true })).toBeVisible()
    await expect(view.getByTitle('Process: Running', { exact: true })).toBeVisible()
    await page.evaluate(
      (data) => {
        ;(window as Window & { emitStatus?: (data: unknown) => void }).emitStatus?.(data)
      },
      { ...session, status: 'stopped', input_needed: false, program_status: { state: 'done' } }
    )
    await expect(view.getByText('Task Done', { exact: true })).toBeVisible()
    await expect(view.getByText('Stopped', { exact: true })).toHaveCount(0)
    await expect(view.getByTitle('Process: Stopped', { exact: true })).toHaveCount(0)
    await expect(view.getByText('Ready for Input', { exact: true })).toHaveCount(0)
    await expect(view.getByText('Input Needed', { exact: true })).toHaveCount(0)
  })
}

// Color regressions are intentionally separate from the SSE lifecycle tests above.

// `lifecycle` becomes the fixture's session status and stays in the API. A
// program report replaces the visible lifecycle badge, so these fixtures never
// render a lifecycle label even when the process is running, stopped, or failed.
const colorReports = [
  { report: { state: 'idle' }, label: 'Idle', lifecycle: 'stopped' },
  {
    report: { state: 'working', progress: 42 },
    label: 'Working 42%',
    lifecycle: 'running',
  },
  {
    report: { state: 'done' },
    label: 'Task Done',
    lifecycle: 'stopped',
  },
  {
    report: { state: 'blocked', kind: 'permission', progress: 67 },
    label: 'Permission Required 67%',
    lifecycle: 'running',
  },
  {
    report: { state: 'error' },
    label: 'Task Error',
    lifecycle: 'failed',
  },
] as const

type ProgramState = (typeof colorReports)[number]['report']['state']

async function mockColorSessions(page: Page) {
  await page.addInitScript(() => {
    // Keep a benign stream open; these fixtures never change their reported state.
    class ColorEventSource extends EventTarget {
      onopen: ((event: Event) => void) | null = null
      onerror: ((event: Event) => void) | null = null
      constructor() {
        super()
        setTimeout(() => this.onopen?.(new Event('open')), 0)
      }
      close() {}
    }
    Object.defineProperty(window, 'EventSource', { value: ColorEventSource })
  })
  const sessions = colorReports.map(({ report, lifecycle }, index) => ({
    id: `color-${report.state}`,
    title: `${report.state} color fixture`,
    tags: [],
    command: 'fixture',
    args: [],
    pid: 1234 + index,
    status: lifecycle,
    created_at: '2026-01-01T00:00:00Z',
    started_at: null,
    ended_at: null,
    cwd: '/tmp',
    input_needed: report.state === 'blocked',
    notifications_enabled: false,
    node: null,
    last_total_bytes: 0,
    program_status: report,
  }))
  await page.route('**/api/auth/status', (route) =>
    route.fulfill({ json: { auth_required: false } })
  )
  await page.route('**/api/nodes', (route) => route.fulfill({ json: [] }))
  await page.route('**/api/push/public-key', (route) =>
    route.fulfill({ json: { public_key: null } })
  )
  await page.route('**/api/sessions?**', (route) =>
    route.fulfill({ json: { items: sessions, total: sessions.length, offset: 0, limit: 15 } })
  )
}

async function readBadgeAppearance(badge: Locator, state: ProgramState) {
  return badge.evaluate((element, state) => {
    type Rgba = [number, number, number, number]
    const canvas = document.createElement('canvas')
    canvas.width = canvas.height = 1
    const context = canvas.getContext('2d', { willReadFrequently: true })
    if (!context) throw new Error('A 2D canvas is required to normalize CSS colors')

    // Canvas accepts both legacy rgb()/rgba() and CSS Color 4 color(srgb ... / alpha)
    // emitted by Tailwind v4 color-mix. Never assume computed colors use one syntax.
    function rgba(cssColor: string): Rgba {
      if (!CSS.supports('color', cssColor)) throw new Error(`Invalid CSS color: ${cssColor}`)
      context!.clearRect(0, 0, 1, 1)
      context!.fillStyle = cssColor
      context!.fillRect(0, 0, 1, 1)
      const [red, green, blue, alpha] = context!.getImageData(0, 0, 1, 1).data
      return [red, green, blue, alpha / 255]
    }

    function over(foreground: Rgba, background: Rgba): Rgba {
      const alpha = foreground[3]
      return [
        foreground[0] * alpha + background[0] * (1 - alpha),
        foreground[1] * alpha + background[1] * (1 - alpha),
        foreground[2] * alpha + background[2] * (1 - alpha),
        1,
      ]
    }

    function luminance(color: Rgba) {
      const linear = color.slice(0, 3).map((channel) => {
        const srgb = channel / 255
        return srgb <= 0.04045 ? srgb / 12.92 : ((srgb + 0.055) / 1.055) ** 2.4
      })
      return linear[0] * 0.2126 + linear[1] * 0.7152 + linear[2] * 0.0722
    }

    const ancestors: Element[] = []
    for (let parent = element.parentElement; parent; parent = parent.parentElement) {
      ancestors.unshift(parent)
    }
    // Start at the canvas, then paint every ancestor from html down to the badge's
    // parent. Transparent spans/table cells must not be mistaken for the surface.
    const parentBackground = ancestors.reduce<Rgba>(
      (background, parent) => over(rgba(getComputedStyle(parent).backgroundColor), background),
      [255, 255, 255, 1]
    )
    const style = getComputedStyle(element)
    const foreground = rgba(style.color)
    const background = rgba(style.backgroundColor)
    const border = rgba(style.borderTopColor)
    const surface = over(background, parentBackground)
    const text = over(foreground, surface)
    const textLuminance = luminance(text)
    const surfaceLuminance = luminance(surface)
    const tokens = getComputedStyle(document.documentElement)
    const accentToken = tokens.getPropertyValue(`--program-status-${state}`).trim()
    const foregroundToken = tokens.getPropertyValue(`--program-status-${state}-foreground`).trim()
    if (!accentToken || !foregroundToken) throw new Error(`Missing ${state} program status tokens`)

    return {
      state,
      foreground,
      background,
      border,
      surface,
      parentBackground,
      accentToken,
      foregroundToken,
      expectedForeground: rgba(`hsl(${foregroundToken})`),
      expectedAccent: rgba(`hsl(${accentToken})`),
      contrast:
        (Math.max(textLuminance, surfaceLuminance) + 0.05) /
        (Math.min(textLuminance, surfaceLuminance) + 0.05),
      ancestorOpacity: ancestors.reduce(
        (opacity, ancestor) => opacity * Number(getComputedStyle(ancestor).opacity),
        Number(style.opacity)
      ),
      fontWeight: Number(style.fontWeight),
      fontSize: Number.parseFloat(style.fontSize),
      fontVariantNumeric: style.fontVariantNumeric,
    }
  }, state)
}

async function expectReadableProgramColors(view: Locator) {
  await expect(view.locator('[title^="Agent-reported (OSC 7501)"]')).toHaveCount(5)
  await expect(view.locator('[title^="Process:"]')).toHaveCount(0)
  const appearances: Awaited<ReturnType<typeof readBadgeAppearance>>[] = []
  for (const { report, label } of colorReports) {
    const badge = view.getByText(label, { exact: true })
    await badge.scrollIntoViewIfNeeded()
    await expect(badge).toBeVisible()
    await expect(badge).toHaveAttribute('title', 'Agent-reported (OSC 7501)')

    // Wait for finite badge/ancestor transitions and entrance animations, not the
    // lifecycle dot's infinite pulse. Cancelled transitions are normal on retheme;
    // all resulting styles are still asserted below.
    await badge.evaluate(async (element) => {
      const animations: Animation[] = []
      for (let current: Element | null = element; current; current = current.parentElement) {
        animations.push(...current.getAnimations())
      }
      await Promise.all(
        animations
          .filter((animation) => animation.effect?.getComputedTiming().iterations !== Infinity)
          .map((animation) => animation.finished.catch(() => {}))
      )
    })
    const initial = await readBadgeAppearance(badge, report.state)
    // Theme changes transition colors. Wait for the token's actual rendered color,
    // not an arbitrary sleep or a transient palette between light and dark.
    await expect
      .poll(async () => (await readBadgeAppearance(badge, report.state)).foreground)
      .toEqual(initial.expectedForeground)
    const appearance = await readBadgeAppearance(badge, report.state)
    expect(appearance.contrast, `${report.state} text/surface contrast`).toBeGreaterThanOrEqual(4.5)
    expect(
      appearance.ancestorOpacity,
      'reported status must not be washed out by inactive-row opacity'
    ).toBe(1)
    const containment = await badge.evaluate((element) => {
      const box = element.getBoundingClientRect()
      const owner = element.closest('td') ?? element.closest('[data-node-swipe-ignore]')
      if (!owner) throw new Error('Badge must belong to a status table cell or card header')
      const bounds = owner.getBoundingClientRect()
      return { left: box.left, right: box.right, ownerLeft: bounds.left, ownerRight: bounds.right }
    })
    expect(containment.left).toBeGreaterThanOrEqual(containment.ownerLeft - 1)
    expect(
      containment.right,
      'badge must not overlap adjacent columns or escape the card'
    ).toBeLessThanOrEqual(containment.ownerRight + 1)
    expect(appearance.fontWeight, `${report.state} text weight`).toBeGreaterThanOrEqual(500)
    expect(appearance.fontSize).toBe(12)
    expect(appearance.fontVariantNumeric).toContain('tabular-nums')
    expect(appearance.background[3]).toBeCloseTo(0.12, 2)
    expect(appearance.border[3]).toBeCloseTo(0.3, 2)
    // A tinted, colored surface and border must use the state accent, not gray or
    // an unrelated lifecycle hue. Unpremultiplying 12% alpha in an 8-bit canvas
    // can round RGB channels by up to five levels.
    for (const color of [appearance.background, appearance.border]) {
      for (let channel = 0; channel < 3; channel++) {
        expect(Math.abs(color[channel] - appearance.expectedAccent[channel])).toBeLessThanOrEqual(5)
      }
    }
    for (const color of [appearance.foreground, appearance.surface]) {
      const channels = color.slice(0, 3)
      expect(
        Math.max(...channels) - Math.min(...channels),
        `${report.state} color chroma`
      ).toBeGreaterThan(2)
    }
    expect(appearance.surface).not.toEqual(appearance.parentBackground)
    appearances.push(appearance)
  }
  expect(new Set(appearances.map(({ foreground }) => foreground.join(','))).size).toBe(5)
  expect(new Set(appearances.map(({ background }) => background.join(','))).size).toBe(5)
  return appearances
}

for (const mobile of [false, true]) {
  for (const theme of ['light', 'dark'] as const) {
    test(`program status colors are readable in ${theme} ${mobile ? 'cards' : 'table'} and system theme`, async ({
      page,
    }, testInfo) => {
      await page.setViewportSize(
        mobile ? { width: 390, height: 844 } : { width: 1280, height: 800 }
      )
      // Explicit themes must win even when the OS preference is the opposite.
      await page.emulateMedia({ colorScheme: theme === 'dark' ? 'light' : 'dark' })
      await mockColorSessions(page)
      await page.goto('/')
      await page.evaluate((theme) => {
        document.documentElement.dataset.theme = theme
      }, theme)
      await expect(page.locator('html')).toHaveAttribute('data-theme', theme)
      const view = mobile ? page.getByTestId('mobile-session-content') : page.getByRole('table')
      const explicit = await expectReadableProgramColors(view)
      await view.getByText('Idle', { exact: true }).scrollIntoViewIfNeeded()
      await page.screenshot({
        path: testInfo.outputPath(`program-status-${theme}-${mobile ? 'cards' : 'table'}.png`),
        fullPage: true,
        animations: 'disabled',
      })

      // Matching system preference must resolve the same accent/foreground tokens
      // and actual pixels, including the system-dark media-query declarations.
      await page.emulateMedia({ colorScheme: theme })
      await page.evaluate(() => document.documentElement.removeAttribute('data-theme'))
      await expect(page.locator('html')).not.toHaveAttribute('data-theme')
      const system = await expectReadableProgramColors(view)
      expect(system).toEqual(explicit)
      await view.getByText('Idle', { exact: true }).scrollIntoViewIfNeeded()
      await page.screenshot({
        path: testInfo.outputPath(
          `program-status-system-${theme}-${mobile ? 'cards' : 'table'}.png`
        ),
        fullPage: true,
        animations: 'disabled',
      })
    })
  }
}
