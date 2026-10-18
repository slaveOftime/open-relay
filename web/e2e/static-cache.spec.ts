import { test, expect } from '@playwright/test'
import { createServer, type ServerResponse } from 'node:http'
import { readFile } from 'node:fs/promises'
import path from 'node:path'
import type { AddressInfo } from 'node:net'

// The ordinary mocked UI suite blocks workers. These tests deliberately run
// the built worker against a real, isolated HTTP server and browser cache.
test.use({ serviceWorkers: 'allow' })

test('the built worker does not shadow revalidating media or authenticated apps', async ({
  page,
}) => {
  let version = 1
  let authorized = true
  const dist = path.resolve('dist')
  const reply = (response: ServerResponse, type: string, body: string | Buffer) => {
    response.setHeader('content-type', type)
    response.end(body)
  }
  const server = createServer((request, response) => {
    void (async () => {
      const pathname = new URL(request.url ?? '/', 'http://localhost').pathname
      if (pathname === '/probe') {
        response.setHeader('cache-control', 'no-store')
        reply(response, 'text/html', '<!doctype html><title>Cache probe</title>')
        return
      }
      if (pathname === '/cache-probe.svg') {
        response.setHeader('cache-control', 'no-cache')
        reply(
          response,
          'image/svg+xml',
          `<svg xmlns="http://www.w3.org/2000/svg" width="${10 + version}" height="10"/>`
        )
        return
      }
      if (pathname === '/apps/cache-probe.js') {
        response.setHeader('cache-control', 'private, no-cache')
        if (!authorized) {
          response.statusCode = 401
          reply(response, 'text/plain', 'unauthorized')
        } else {
          reply(response, 'application/javascript', `window.cacheProbeVersion = ${version};`)
        }
        return
      }
      const file = path.resolve(dist, `.${pathname}`)
      if (!file.startsWith(`${dist}${path.sep}`)) {
        response.statusCode = 404
        response.end()
        return
      }
      try {
        const bytes = await readFile(file)
        response.setHeader('cache-control', 'no-cache')
        reply(
          response,
          file.endsWith('.js') ? 'application/javascript' : 'application/octet-stream',
          bytes
        )
      } catch {
        response.statusCode = 404
        response.end()
      }
    })().catch(() => {
      response.statusCode = 500
      response.end()
    })
  })
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve))
  const origin = `http://127.0.0.1:${(server.address() as AddressInfo).port}`
  try {
    await page.goto(`${origin}/probe`)
    await page.evaluate(async () => {
      await navigator.serviceWorker.register('/sw.js')
      await navigator.serviceWorker.ready
      if (!navigator.serviceWorker.controller) {
        await new Promise<void>((resolve) => {
          navigator.serviceWorker.addEventListener('controllerchange', () => resolve(), {
            once: true,
          })
        })
      }
    })
    expect(await page.evaluate(() => navigator.serviceWorker.controller?.scriptURL)).toBe(
      `${origin}/sw.js`
    )
    await page.reload()
    await expect(page).toHaveTitle('Cache probe')
    // The worker still serves the revisioned build precache; runtime-cache
    // removal must not accidentally disable registration or all fetch handling.
    const manifest = JSON.parse(
      await readFile(path.join(dist, '.vite/manifest.json'), 'utf8')
    ) as Record<string, { file: string }>
    const bundledScript = Object.values(manifest).find((entry) => entry.file.endsWith('.js'))!.file
    const precached = page.waitForResponse(`${origin}/${bundledScript}`)
    expect(
      await page.evaluate(async (file) => (await fetch(`/${file}`)).status, bundledScript)
    ).toBe(200)
    expect((await precached).fromServiceWorker()).toBe(true)
    const imageWidth = () =>
      page.evaluate(
        () =>
          new Promise<number>((resolve, reject) => {
            const image = new Image()
            image.onload = () => resolve(image.naturalWidth)
            image.onerror = () => reject(new Error('cache probe image failed'))
            image.src = '/cache-probe.svg'
            document.body.append(image)
          })
      )
    const loadScript = () =>
      page.evaluate(
        () =>
          new Promise<boolean>((resolve) => {
            const script = document.createElement('script')
            script.onload = () => resolve(true)
            script.onerror = () => resolve(false)
            script.src = '/apps/cache-probe.js'
            document.body.append(script)
          })
      )
    const scriptVersion = () =>
      page.evaluate(() => (window as Window & { cacheProbeVersion?: number }).cacheProbeVersion)

    expect(await imageWidth()).toBe(11)
    expect(await loadScript()).toBe(true)
    expect(await scriptVersion()).toBe(1)
    // Seed the old worker's named caches to model an upgrade deterministically.
    // A current worker must leave these stale runtime entries unused.
    await page.evaluate(async () => {
      await (await caches.open('media-assets')).add('/cache-probe.svg')
      await (await caches.open('static-assets')).add('/apps/cache-probe.js')
    })
    version = 2
    expect.soft(await imageWidth()).toBe(12)
    expect(await loadScript()).toBe(true)
    expect.soft(await scriptVersion()).toBe(2)
    authorized = false
    expect(await loadScript()).toBe(false)
  } finally {
    server.closeAllConnections()
    await new Promise<void>((resolve, reject) =>
      server.close((error) => (error ? reject(error) : resolve()))
    )
  }
})
