import { defineConfig, devices } from '@playwright/test'

/**
 * The reconnect and quick-key paths exist for iOS standalone behaviour, so the
 * suite runs on Chromium *and* a mobile-Safari profile. CI installs only the
 * Chromium browser, so the webkit project is opt-in locally with
 * `--project=mobile-safari` (see ci-web.yml).
 */
const MOBILE_SAFARI = {
  ...devices['iPhone 13'],
  defaultBrowserType: 'webkit' as const,
}

export default defineConfig({
  testDir: './e2e',
  fullyParallel: true,
  retries: 0,
  reporter: 'list',
  use: {
    baseURL: 'http://127.0.0.1:4173',
    serviceWorkers: 'block',
    // A failure without a trace is a failure you cannot act on, and this suite
    // is the only thing that catches the browser-level regressions tsc and the
    // unit tests happily pass (a second terminal mount, a missing header label,
    // a list that applied a stale response).
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  // Locally a flake fails loudly and stays fast; CI gets the retry so one
  // slow worker does not turn a green build red. The traces above are what make
  // the retry honest rather than a way to hide a real bug.
  retries: process.env.CI ? 2 : 0,
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
    {
      name: 'mobile-safari',
      // Only the attach specs, deliberately. The others either assume the
      // desktop table (`sessions.spec.ts` uses `locator('table')`, which does
      // not exist at 390px where the card list renders instead) or synthesize
      // `Touch` objects (`sessions-delete-swipe.spec.ts`), which WebKit rejects
      // with "Illegal constructor". Both would need the specs themselves
      // reworked for an iOS viewport, which is a separate job from adding the
      // engine to the matrix - what iOS adds here is coverage of the iOS-only
      // attach and keyboard paths.
      testMatch: /(attach|quick-keys)[^/]*\.spec\.ts/,
      use: { ...MOBILE_SAFARI },
    },
  ],
  webServer: {
    command: 'npm run build && npm run preview -- --host 127.0.0.1 --port 4173',
    url: 'http://127.0.0.1:4173',
    reuseExistingServer: true,
    timeout: 120000,
  },
})
