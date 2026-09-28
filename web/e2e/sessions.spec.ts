import { test, expect } from '@playwright/test'

test('renders sessions page with mocked API data', async ({ page }) => {
  const item = {
    id: '1234567-89ab-cdef-0123-456789abcdef',
    title: 'demo session',
    tags: [],
    command: 'bash',
    args: [],
    pid: 1234,
    status: 'running',
    age: '2m',
    created_at: new Date().toISOString(),
    cwd: '/tmp',
    input_needed: false,
    node: null,
    last_total_bytes: 0,
  }
  await page.route('**/api/auth/status', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ auth_required: false }),
    })
  })

  await page.route('**/api/nodes', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify([]),
    })
  })

  await page.route('**/api/push/public-key', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ public_key: null }),
    })
  })

  await page.route('**/api/push/subscriptions', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ ok: true, deleted: false }),
    })
  })

  await page.route('**/api/sessions/events**', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'text/event-stream',
      body: `event: snapshot\ndata: ${JSON.stringify([item])}\n\n`,
    })
  })

  await page.route('**/api/sessions**', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        items: [item],
        total: 1,
        offset: 0,
        limit: 15,
      }),
    })
  })

  await page.goto('/')

  await expect(page.locator('header').first()).toContainText('Open Relay')
  await expect(page.locator('table').getByText('demo session')).toBeVisible()
})

test('switching nodes ignores old list responses and keeps the live indicator stable', async ({
  page,
}) => {
  await page.addInitScript(() => {
    // Keep the mocked stream open while the table and its activity cells render.
    class OpenEventSource extends EventTarget {
      static latest: OpenEventSource | null = null
      onopen: ((event: Event) => void) | null = null
      onerror: ((event: Event) => void) | null = null
      constructor(url: string) {
        super()
        void url
        OpenEventSource.latest = this
        setTimeout(() => this.onopen?.(new Event('open')), 0)
      }
      close() {}
    }
    Object.defineProperty(window, 'EventSource', { value: OpenEventSource })
    ;(window as Window & { emitSessionUpdate?: (data: unknown) => void }).emitSessionUpdate = (
      data
    ) =>
      OpenEventSource.latest?.dispatchEvent(
        new MessageEvent('session_updated', { data: JSON.stringify(data) })
      )
  })

  await page.route('**/api/auth/status', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '{"auth_required":false}' })
  )
  await page.route('**/api/nodes', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify([{ name: 'worker-a' }, { name: 'worker-b' }]),
    })
  )
  await page.route('**/api/push/public-key', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '{"public_key":null}' })
  )

  const session = (title: string, node: string | null) => ({
    id: `${title.replaceAll(' ', '-')}-id`,
    title,
    tags: [],
    command: 'bash',
    args: [],
    pid: 1234,
    status: 'running',
    created_at: new Date().toISOString(),
    cwd: '/tmp',
    input_needed: false,
    node,
    last_total_bytes: 0,
  })
  let releaseLocal: (() => void) | undefined
  let releaseWorkerA: (() => void) | undefined
  let listRequests = 0
  await page.route('**/api/sessions?**', async (route) => {
    listRequests += 1
    const node = new URL(route.request().url()).searchParams.get('node')
    if (node === null)
      await new Promise<void>((resolve) => {
        releaseLocal = resolve
      })
    if (node === 'worker-a')
      await new Promise<void>((resolve) => {
        releaseWorkerA = resolve
      })
    const title = node === null ? 'local session' : `${node} session`
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ items: [session(title, node)], total: 1, offset: 0, limit: 15 }),
    })
  })

  await page.setViewportSize({ width: 1280, height: 800 })
  await page.goto('/')
  await expect.poll(() => Boolean(releaseLocal)).toBe(true)
  const nodeSelector = page.locator('header').getByRole('combobox').last()
  await expect(nodeSelector).toBeVisible()
  await nodeSelector.click()
  await page.getByRole('option', { name: 'worker-a' }).click()
  await expect.poll(() => Boolean(releaseWorkerA)).toBe(true)
  await nodeSelector.click()
  await page.getByRole('option', { name: 'worker-b' }).click()
  await expect(page.locator('table').getByText('worker-b session')).toBeVisible()
  await expect(page.getByText('Live', { exact: true })).toBeVisible()

  const workerAResponse = page.waitForResponse(
    (response) =>
      response.url().includes('/api/sessions?') &&
      new URL(response.url()).searchParams.get('node') === 'worker-a'
  )
  const localResponse = page.waitForResponse(
    (response) =>
      response.url().includes('/api/sessions?') && !new URL(response.url()).searchParams.has('node')
  )
  releaseWorkerA?.()
  releaseLocal?.()
  await Promise.all([workerAResponse, localResponse])
  // Let both fetch continuations and their React updates commit before asserting.
  await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => resolve())))
  await expect(page.locator('table').getByText('worker-b session')).toBeVisible()
  const activity = page.locator('table span[aria-label*="activity"]').first()
  await expect(activity).toHaveAttribute('aria-label', /Recent: 0 B\/s/)
  await page.evaluate(() => {
    const path = document.querySelector('table svg path[stroke-width="2"]')
    if (!path) throw new Error('Missing sparkline path')
    const monitor = { changes: 0 }
    new MutationObserver((records) => {
      monitor.changes += records.length
    }).observe(path, {
      attributes: true,
      attributeFilter: ['d'],
    })
    ;(window as Window & { sparklineMonitor?: typeof monitor }).sparklineMonitor = monitor
  })
  // A flat graph must not schedule a permanent idle animation loop.
  await page.waitForTimeout(100)
  expect(
    await page.evaluate(
      () =>
        (window as Window & { sparklineMonitor?: { changes: number } }).sparklineMonitor?.changes
    )
  ).toBe(0)
  // A foreign node's session with the same id must not alter this chart.
  const workerB = session('worker-b session', 'worker-b')
  const requestsBeforeActivity = listRequests
  await page.evaluate(
    (data) => {
      ;(window as Window & { emitSessionUpdate?: (data: unknown) => void }).emitSessionUpdate?.(
        data
      )
    },
    { ...workerB, node: 'worker-a', last_total_bytes: 2048 }
  )
  await expect(activity).toHaveAttribute('aria-label', /Recent: 0 B\/s/)
  await page.evaluate(
    (data) => {
      ;(window as Window & { emitSessionUpdate?: (data: unknown) => void }).emitSessionUpdate?.(
        data
      )
    },
    { ...workerB, last_total_bytes: 1024 }
  )
  await expect(activity).toHaveAttribute('aria-label', /Recent: 512 B\/s/)
  await expect
    .poll(
      () =>
        page.evaluate(
          () =>
            (window as Window & { sparklineMonitor?: { changes: number } }).sparklineMonitor
              ?.changes ?? 0
        ),
      { timeout: 1_000 }
    )
    .toBeGreaterThan(3)
  const graphPath = page.locator('table svg path[stroke-width="2"]').first()
  const firstPath = await graphPath.getAttribute('d')
  // Opening the next empty 500 ms bucket must not draw a drop to the baseline.
  await expect
    .poll(
      async () => {
        const path = await graphPath.getAttribute('d')
        if (!path || path === firstPath) return false
        const points = [...path.matchAll(/[ML] [\d.]+ ([\d.]+)/g)]
        const previousY = Number(points.at(-2)?.[1])
        const latestY = Number(points.at(-1)?.[1])
        return latestY === previousY && latestY < 19
      },
      { timeout: 2_000 }
    )
    .toBe(true)
  expect(listRequests).toBe(requestsBeforeActivity)
  await expect(page.locator('table').getByText('worker-a session')).toHaveCount(0)
  await expect(page.locator('table').getByText('local session')).toHaveCount(0)
  await expect(page.getByText('Live', { exact: true })).toBeVisible()

  await page.setViewportSize({ width: 375, height: 812 })
  await expect(page.getByText('Live', { exact: true })).toBeVisible()
  await expect(page.getByText('worker-b session').first()).toBeVisible()
})

test('duplicate force-removes the source only after create and retries removal without cloning twice', async ({
  page,
}) => {
  await page.addInitScript(() => {
    class OpenEventSource extends EventTarget {
      onopen: ((event: Event) => void) | null = null
      constructor() {
        super()
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
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: '[{"name":"worker-a","connected":true}]',
    })
  )
  await page.route('**/api/push/public-key', (route) =>
    route.fulfill({ status: 200, contentType: 'application/json', body: '{"public_key":null}' })
  )
  const source = {
    id: 'source-id',
    node: 'worker-a',
    title: 'original session',
    tags: [],
    command: 'bash',
    args: ['-c', 'echo hi'],
    pid: 1234,
    status: 'running',
    created_at: new Date().toISOString(),
    cwd: '/tmp',
    input_needed: false,
    notifications_enabled: true,
    last_total_bytes: 0,
  }
  await page.route('**/api/sessions?**', (route) =>
    route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({ items: [source], total: 1, offset: 0, limit: 15 }),
    })
  )
  let releaseCreate: (() => void) | undefined
  let createCount = 0
  let removeCount = 0
  await page.route('**/api/sessions', async (route) => {
    expect(route.request().method()).toBe('POST')
    expect(route.request().postDataJSON()).toMatchObject({ node: 'worker-a', cmd: 'bash' })
    createCount += 1
    if (createCount === 4 || createCount === 5) {
      await route.fulfill({
        status: createCount === 4 ? 400 : 201,
        contentType: 'application/json',
        body: createCount === 4 ? '{"error":"unable to start"}' : '{}',
      })
      return
    }
    await new Promise<void>((resolve) => {
      releaseCreate = resolve
    })
    await route.fulfill({
      status: 201,
      contentType: 'application/json',
      body: JSON.stringify({ session_id: createCount === 2 ? 'source-id' : 'copy-id' }),
    })
  })
  await page.route('**/api/sessions/source-id?**', (route) => {
    expect(route.request().method()).toBe('DELETE')
    const params = new URL(route.request().url()).searchParams
    expect(params.get('force')).toBe('true')
    expect(params.get('node')).toBe('worker-a')
    removeCount += 1
    return route.fulfill({
      status: removeCount === 1 ? 503 : 200,
      contentType: 'application/json',
      body: removeCount === 1 ? '{"error":"disk busy"}' : '{"removed":true}',
    })
  })

  await page.goto('/?node=worker-a')
  await expect(page.locator('table').getByText('original session')).toBeVisible()
  await page.locator('table').getByRole('button', { name: 'Run Again' }).click()
  const toggle = page.getByRole('switch', { name: 'Remove original session' })
  await expect(toggle).toHaveAttribute('data-state', 'unchecked')
  await toggle.click()
  await expect(toggle).toHaveAttribute('data-state', 'checked')
  const destructiveBackground = await page.evaluate(() => {
    const probe = document.createElement('div')
    probe.style.backgroundColor = 'hsl(var(--destructive))'
    document.body.appendChild(probe)
    const color = getComputedStyle(probe).backgroundColor
    probe.remove()
    return color
  })
  await expect
    .poll(() => toggle.evaluate((element) => getComputedStyle(element).backgroundColor))
    .toBe(destructiveBackground)
  await page.getByRole('button', { name: 'Start & remove original' }).click()
  await expect.poll(() => createCount).toBe(1)
  expect(removeCount).toBe(0)
  releaseCreate?.()
  await expect(page.getByRole('alert')).toContainText(
    'New session copy-id started, but original source-id was not removed: disk busy'
  )
  await page.getByRole('button', { name: 'Retry removal' }).click()
  await expect.poll(() => removeCount).toBe(2)
  await expect(page.getByRole('dialog')).toHaveCount(0)
  expect(createCount).toBe(1)

  // A normal new session has no destructive toggle.
  await page.getByRole('button', { name: 'New' }).click()
  await expect(page.getByRole('switch', { name: 'Remove original session' })).toHaveCount(0)
  await page.getByRole('button', { name: 'Cancel' }).click()

  // Never force-remove a returned id if it is the source on the same node.
  await page.locator('table').getByRole('button', { name: 'Run Again' }).click()
  await toggle.click()
  await page.getByRole('button', { name: 'Start & remove original' }).click()
  await expect.poll(() => createCount).toBe(2)
  releaseCreate?.()
  await expect(page.getByRole('alert')).toContainText('new session has the same ID as the original')
  await expect(page.getByRole('button', { name: 'Retry removal' })).toBeDisabled()
  expect(removeCount).toBe(2)
  await toggle.click()
  await page.getByRole('button', { name: 'Keep original' }).click()
  await expect(page.getByRole('dialog')).toHaveCount(0)

  // The same Run Again flow defaults to keeping the source if left unchecked.
  await page.locator('table').getByRole('button', { name: 'Run Again' }).click()
  await expect(toggle).toHaveAttribute('data-state', 'unchecked')
  await page.getByRole('button', { name: 'Start Session' }).click()
  await expect.poll(() => createCount).toBe(3)
  releaseCreate?.()
  await expect(page.getByRole('dialog')).toHaveCount(0)
  expect(removeCount).toBe(2)

  // If creating the replacement fails, the original must never be removed.
  await page.locator('table').getByRole('button', { name: 'Run Again' }).click()
  await toggle.click()
  await page.getByRole('button', { name: 'Start & remove original' }).click()
  await expect(page.getByRole('alert')).toContainText('unable to start')
  expect(createCount).toBe(4)
  expect(removeCount).toBe(2)
  await page.getByRole('button', { name: 'Cancel' }).click()

  // A malformed create response cannot authorize deleting the source or another start.
  await page.locator('table').getByRole('button', { name: 'Run Again' }).click()
  await toggle.click()
  await page.getByRole('button', { name: 'Start & remove original' }).click()
  await expect(page.getByRole('alert')).toContainText('Server did not return a new session ID')
  await expect(page.getByRole('button', { name: 'Start & remove original' })).toBeDisabled()
  expect(createCount).toBe(5)
  expect(removeCount).toBe(2)
  await page.locator('form').getByRole('button', { name: 'Close' }).click()
})
