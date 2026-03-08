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
        short_name: 'Open Relay',
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
        enabled: true,
      },
      workbox: {
        navigateFallback: '/index.html',
        navigateFallbackAllowlist: [/^\//],
        navigateFallbackDenylist: [/^\/api\//, /^\/sw-push\.js$/],
        // Do NOT add runtimeCaching rules for /api/ routes.
        //
        // On iOS PWA (standalone mode) WebKit routes WebSocket upgrade requests
        // through the service worker fetch event.  Any Workbox strategy that
        // calls fetch(event.request) for a WS upgrade (even NetworkOnly) either
        // adds inter-process latency or breaks the upgrade entirely because
        // window.fetch() cannot complete a protocol upgrade.
        //
        // Leaving no matching route for /api/ means Workbox never calls
        // respondWith(), so the browser handles those requests natively — the
        // same fast path used in normal iOS Safari.  The pattern below was
        // previously attempted but the $ anchor excluded all URLs with query
        // strings (?token=, ?node=, ...), silently falling through to the old
        // NetworkFirst rule and breaking WS connections with those params.
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
    port: 8060,
    host: '127.0.0.1',
    allowedHosts: ['host.docker.internal'],
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:15443',
        changeOrigin: true,
        ws: true,
      },
    },
  },
  test: {
    environment: 'node',
    include: ['src/**/*.test.ts'],
  },
  build: {
    target: 'es2020',
    sourcemap: false,
    minify: 'esbuild',
    cssMinify: true,
    reportCompressedSize: true,
    rollupOptions: {
      output: {
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
  esbuild: {
    // Trim production output while preserving app behavior.
    drop: ['debugger'],
  },
})
