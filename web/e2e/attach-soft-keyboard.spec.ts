import { devices, expect, test, type CDPSession, type Page } from '@playwright/test'

/**
 * Android soft-keyboard coverage for attach mode.
 *
 * The reported regression: on a phone browser, typing `"` and then `d` sent
 * `"""d"`. Android IMEs send their characters through the hidden textarea, and
 * xterm's diff of that textarea (`newValue.replace(oldValue, '')`) only handles
 * append-only edits; a keyboard that edits in place (an auto-paired quote, a
 * correction, an accepted suggestion) makes the diff miss, and xterm replays
 * the whole buffer on every later key.
 *
 * `components/terminal/soft-keyboard-input.ts` now lets the IME own the
 * textarea and mirrors its value diffs into terminal input: the field keeps
 * the typed word, so the IME's suggestions keep replacing the right text, and
 * each edit is translated into backspaces plus the replacement tail. These
 * tests pin what the mocked attach socket actually receives.
 *
 * The events come from CDP's `Input` domain, which is Chromium-only — the
 * `mobile-safari` project skips this file rather than faking the shapes.
 */

const SESSION_ID = 'attach-session'

/** A Pixel 7 reports Android and has touch, so the repair must attach. */
test.use({ ...devices['Pixel 7'] })

test.skip(({ browserName }) => browserName !== 'chromium', 'CDP Input domain is Chromium-only')

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

/** Stubs everything but the attach socket, recording every input frame sent. */
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

async function openAttach(page: Page, inputs: string[]): Promise<void> {
  await stubApi(page, inputs)
  await page.goto(`/session/${SESSION_ID}?mode=attach`)
  await expect(page.locator('#main-container').getByText('Attached')).toBeVisible()
  await page.locator('.xterm').click({ position: { x: 40, y: 40 } })
  await expect(page.locator('.xterm-helper-textarea')).toBeFocused()
}

/** One soft key: the IME's placeholder keydown, the text, then keyup. */
async function softKey(client: CDPSession, text: string): Promise<void> {
  await client.send('Input.dispatchKeyEvent', {
    type: 'rawKeyDown',
    key: 'Unidentified',
    windowsVirtualKeyCode: 229,
    nativeVirtualKeyCode: 229,
  })
  await client.send('Input.insertText', { text })
  await client.send('Input.dispatchKeyEvent', {
    type: 'keyUp',
    key: 'Unidentified',
    windowsVirtualKeyCode: 229,
    nativeVirtualKeyCode: 229,
  })
}

/** The hidden textarea's value as the page sees it. */
function textareaValue(page: Page): Promise<string> {
  return page.evaluate(() => {
    const textarea = document.querySelector('.xterm-helper-textarea') as HTMLTextAreaElement | null
    return textarea?.value ?? '<none>'
  })
}

/**
 * One IME replacement: a suggestion tap or an autocorrection rewrites the
 * field's value. No keydown precedes a tap on the suggestion strip, so the
 * edit is reported on its own.
 */
async function softReplacement(page: Page, data: string): Promise<void> {
  await page.evaluate((text) => {
    const textarea = document.querySelector('.xterm-helper-textarea') as HTMLTextAreaElement
    textarea.value = text
    textarea.setSelectionRange(text.length, text.length)
    textarea.dispatchEvent(
      new InputEvent('input', {
        bubbles: true,
        inputType: 'insertReplacementText',
        data: text,
      })
    )
  }, data)
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms))

test('a keyboard edit xterm cannot diff is sent once, never replayed', async ({ page }) => {
  const inputs: string[] = []
  await openAttach(page, inputs)
  const client = await page.context().newCDPSession(page)

  // What an auto-paired quote looks like: the pair appears in the field with
  // no event at all. The next diff must send it exactly once; before the
  // repair, every later key re-sent the whole buffer.
  await page.evaluate(() => {
    const textarea = document.querySelector('.xterm-helper-textarea') as HTMLTextAreaElement
    textarea.value = '""'
    textarea.setSelectionRange(1, 1)
  })

  await softKey(client, 'd')
  await expect.poll(() => inputs.length).toBe(1)
  expect(inputs).toEqual(['"d"'])

  // The auto-pair left the caret before the closing quote, so the next key
  // is a minimal edit — erase the quote, type the character and the quote
  // back — not a replay of the line.
  await softKey(client, 'x')
  await expect.poll(() => inputs.length).toBe(2)
  expect(inputs).toEqual(['"d"', '\x7fx"'])
  expect(await textareaValue(page)).toBe('"dx"')

  await client.detach()
})

test('each soft key sends its own character exactly once and keeps the buffer', async ({
  page,
}) => {
  const inputs: string[] = []
  await openAttach(page, inputs)
  const client = await page.context().newCDPSession(page)

  for (const text of ['a', 'b', '"', 'd', 'x']) {
    const before = inputs.length
    await softKey(client, text)
    await expect.poll(() => inputs.length).toBe(before + 1)
  }

  expect(inputs).toEqual(['a', 'b', '"', 'd', 'x'])
  // The field is the IME's buffer now: it keeps the line it typed so a
  // suggestion can replace the right word.
  expect(await textareaValue(page)).toBe('ab"dx')

  await client.detach()
})

test('a tapped suggestion extends the typed word instead of appending it', async ({ page }) => {
  const inputs: string[] = []
  await openAttach(page, inputs)
  const client = await page.context().newCDPSession(page)

  for (const text of ['r', 'e', 'f']) {
    const before = inputs.length
    await softKey(client, text)
    await expect.poll(() => inputs.length).toBe(before + 1)
  }

  // The reported `refrefactor`: the accepted suggestion must only extend the
  // word already sent (`actor`), never arrive as a second full word.
  await softReplacement(page, 'refactor')
  await expect.poll(() => inputs.length).toBe(4)
  expect(inputs).toEqual(['r', 'e', 'f', 'actor'])
  expect(await textareaValue(page)).toBe('refactor')

  await client.detach()
})

test('space accepting the suggestion as typed does not send the word again', async ({ page }) => {
  const inputs: string[] = []
  await openAttach(page, inputs)
  const client = await page.context().newCDPSession(page)

  for (const text of ['g', 'o']) {
    const before = inputs.length
    await softKey(client, text)
    await expect.poll(() => inputs.length).toBe(before + 1)
  }

  // The reported `gogo`: pressing space arrives as a replacement carrying
  // the word as typed, followed by the space itself.
  await softReplacement(page, 'go')
  await softKey(client, ' ')
  await expect.poll(() => inputs.length).toBe(3)
  expect(inputs).toEqual(['g', 'o', ' '])
  expect(await textareaValue(page)).toBe('go ')

  await client.detach()
})

test('a comma after a suggestion is sent exactly once', async ({ page }) => {
  const inputs: string[] = []
  await openAttach(page, inputs)
  const client = await page.context().newCDPSession(page)

  for (const text of ['r', 'e', 'f']) {
    const before = inputs.length
    await softKey(client, text)
    await expect.poll(() => inputs.length).toBe(before + 1)
  }

  await softReplacement(page, 'refactor')
  await expect.poll(() => inputs.length).toBe(4)

  await softKey(client, ',')
  await expect.poll(() => inputs.length).toBe(5)
  expect(inputs).toEqual(['r', 'e', 'f', 'actor', ','])
  expect(await textareaValue(page)).toBe('refactor,')

  await client.detach()
})

test('a composition commits once and leaves its text in the buffer', async ({ page }) => {
  const inputs: string[] = []
  await openAttach(page, inputs)
  const client = await page.context().newCDPSession(page)

  // The Android Gboard shape: the composing text grows in place, then the
  // word is committed (see the probes in the issue's investigation).
  await client.send('Input.imeSetComposition', { text: 'n', selectionStart: 1, selectionEnd: 1 })
  await client.send('Input.imeSetComposition', {
    text: 'nihao',
    selectionStart: 5,
    selectionEnd: 5,
  })
  await client.send('Input.insertText', { text: '你好' })
  await expect.poll(() => inputs.length).toBe(1)
  expect(inputs).toEqual(['你好'])

  // xterm's deferred read owns the commit; the mirror adopts it afterwards,
  // so a following soft key adds only its own character.
  await expect.poll(() => textareaValue(page)).toBe('你好')
  await softKey(client, 'x')
  await expect.poll(() => inputs.length).toBe(2)
  expect(inputs).toEqual(['你好', 'x'])
  expect(await textareaValue(page)).toBe('你好x')

  await client.detach()
})

test('a composition after typed text commits only the composed word', async ({ page }) => {
  const inputs: string[] = []
  await openAttach(page, inputs)
  const client = await page.context().newCDPSession(page)

  for (const text of ['h', 'i']) {
    const before = inputs.length
    await softKey(client, text)
    await expect.poll(() => inputs.length).toBe(before + 1)
  }

  // The field already holds a word when the IME starts composing. xterm
  // records the field length at compositionstart, so its deferred read sends
  // only the composed text, never the word that was already typed.
  await client.send('Input.imeSetComposition', { text: '你', selectionStart: 1, selectionEnd: 1 })
  await client.send('Input.imeSetComposition', {
    text: '你好',
    selectionStart: 2,
    selectionEnd: 2,
  })
  await client.send('Input.insertText', { text: '你好' })

  await expect.poll(() => inputs.length).toBe(3)
  expect(inputs).toEqual(['h', 'i', '你好'])
  expect(await textareaValue(page)).toBe('hi你好')

  await softKey(client, 'x')
  await expect.poll(() => inputs.length).toBe(4)
  expect(inputs).toEqual(['h', 'i', '你好', 'x'])
  expect(await textareaValue(page)).toBe('hi你好x')

  await client.detach()
})

test('a hardware key is not sent again by the repair', async ({ page }) => {
  const inputs: string[] = []
  await openAttach(page, inputs)
  const client = await page.context().newCDPSession(page)

  await client.send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'z', text: 'z' })
  await client.send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'z' })

  // xterm's keypress path owns a real key: the repair must not intercept the
  // insertion that accompanies it.
  await expect.poll(() => inputs.length).toBe(1)
  // A double send would arrive right behind the first frame; give it time.
  await sleep(50)
  expect(inputs).toEqual(['z'])

  await client.detach()
})
