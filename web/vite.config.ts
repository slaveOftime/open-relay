/// <reference types="vitest" />
import { defineConfig } from 'vitest/config'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import { VitePWA } from 'vite-plugin-pwa'
import path from 'path'

// https://vite.dev/config/
export default defineConfig({
  plugins: [
    react(),
    tailwindcss(),
    VitePWA({
      registerType: 'autoUpdate',
      includeAssets: ['touch-icon.png', 'pwa-192x192.png', 'pwa-512x512.png'],
      manifest: {
        name: 'Open Relay',
        short_name: 'Oly',
        description: 'Persistent PTY session manager',
        start_url: '/',
        scope: '/',
        theme_color: '#18181b',
        background_color: '#18181b',
        display: 'standalone',
        icons: [
          { src: 'pwa-192x192.png', sizes: '192x192', type: 'image/png' },
          { src: 'pwa-512x512.png', sizes: '512x512', type: 'image/png', purpose: 'any maskable' },
        ],
      },
      devOptions: {
        enabled: false,
      },
      workbox: {
        globIgnores: ['**/index.html'],
        navigateFallback: null,
        // Do NOT add runtimeCaching rules for /api/ routes.
        //
        // On iOS PWA (standalone mode) WebKit routes WebSocket upgrade requests
        // through the service worker fetch event.  Any Workbox strategy that
        // calls fetch(event.request) for a WS upgrade (even NetworkOnly) either
        // adds inter-process latency or breaks the upgrade entirely because
        // window.fetch() cannot complete a protocol upgrade.
        //
        // The daemon already serves index.html as the SPA fallback, so keeping
        // HTML out of the precache and disabling Workbox's navigation fallback
        // avoids stale shell responses for / and keeps the entry document on the
        // normal network path. Leaving no matching route for /api/ means Workbox
        // never calls respondWith(), so the browser handles those requests
        // natively.
        // Runtime Cache Storage strategies do not honor HTTP no-cache/private
        // directives. They can shadow mutable public media or serve trusted
        // app scripts after auth is revoked. Only the build's revisioned
        // precache is worker-owned; all other requests use the browser's HTTP
        // cache and the daemon's validators. Old runtime caches remain unused.
        runtimeCaching: [],
      },
    }),
  ],
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  server: {
    port: 8061,
    host: '127.0.0.1',
    allowedHosts: ['host.docker.internal'],
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:8060',
        changeOrigin: true,
        ws: true,
        // The backend validates the WebSocket handshake Origin against the
        // Host it sees or X-Forwarded-Host (ADR-0007). changeOrigin rewrites
        // Host to the backend address, so mirror the dev server's own
        // authority into X-Forwarded-Host on every WS upgrade; that keeps
        // terminal attach working on any dev port without weakening the
        // check in production (which has no proxy in front).
        configure: (proxy) => {
          proxy.on('proxyReqWs', (proxyReq, req) => {
            if (req.headers.host) proxyReq.setHeader('x-forwarded-host', req.headers.host)
          })
        },
      },
      '/apps': {
        target: 'http://127.0.0.1:15443',
        changeOrigin: true,
        ws: true,
        configure: (proxy) => {
          proxy.on('proxyReqWs', (proxyReq, req) => {
            if (req.headers.host) proxyReq.setHeader('x-forwarded-host', req.headers.host)
          })
        },
      },
    },
  },
  test: {
    environment: 'node',
    // .tsx test files exist (StatusBadge.test.tsx) and were silently skipped by
    // the old `*.test.ts` glob, so match both extensions now.
    include: ['src/**/*.test.{ts,tsx}'],
  },
  build: {
    // Proves build provenance to the daemon: copied public files and operator
    // overrides must not earn immutable caching from filename shape alone.
    manifest: true,
    target: 'es2020',
    sourcemap: false,
    minify: 'oxc',
    cssMinify: true,
    reportCompressedSize: true,
    rollupOptions: {
      output: {
        // Every bundle file must keep a content hash in its name: the daemon
        // caches manifest-listed `assets/<name>-<hash8>.<ext>` immutably in
        // release builds (see src/http/static_files.rs).
        // Vite's defaults already hash these, but pinning the pattern here
        // keeps the contract explicit and covered by review.
        entryFileNames: 'assets/[name]-[hash:8].js',
        chunkFileNames: 'assets/[name]-[hash:8].js',
        assetFileNames: 'assets/[name]-[hash:8][extname]',
        // Keep heavy runtime deps in separate chunks for better browser caching.
        manualChunks(id) {
          if (id.includes('node_modules/@xterm/')) return 'xterm'
          if (id.includes('node_modules/react') || id.includes('node_modules/react-dom'))
            return 'react-vendor'
          if (id.includes('node_modules/react-router-dom')) return 'router-vendor'
          if (id.includes('node_modules/@radix-ui/')) return 'radix-vendor'
        },
      },
    },
  },
})
