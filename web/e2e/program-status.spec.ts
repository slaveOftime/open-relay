import { test, expect } from '@playwright/test'

for (const mobile of [false, true]) {
  test(`OSC 7501 status changes update ${mobile ? 'cards' : 'rows'} without changing lifecycle`, async ({
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
      [{ state: 'error' }, 'Task Error'],
      [{ state: 'idle' }, 'Idle'],
    ] as const) {
      await page.evaluate(
        (data) => {
          ;(window as Window & { emitStatus?: (data: unknown) => void }).emitStatus?.(data)
        },
        { ...session, input_needed: report.state === 'blocked', program_status: report }
      )
      await expect(view.getByText(label, { exact: true })).toBeVisible()
      await expect(view.getByText('Running', { exact: true })).toBeVisible()
    }
    await page.evaluate(
      (data) => {
        ;(window as Window & { emitStatus?: (data: unknown) => void }).emitStatus?.(data)
      },
      { ...session, program_status: null }
    )
    await expect(view.getByText('Idle', { exact: true })).toHaveCount(0)
    await expect(view.getByText('Running', { exact: true })).toBeVisible()
  })
}
