import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { registerSW } from 'virtual:pwa-register'
import './index.css'
import App from './App'
import ErrorBoundary from '@/components/ErrorBoundary'

const LAST_ROUTE_KEY = 'open-relay:last-route'

function isStandalonePwa(): boolean {
  const nav = window.navigator as Navigator & { standalone?: boolean }
  return window.matchMedia('(display-mode: standalone)').matches || nav.standalone === true
}

function restoreLaunchRoute() {
  if (!isStandalonePwa()) return
  if (window.location.pathname !== '/' || window.location.search || window.location.hash) return
  const saved = localStorage.getItem(LAST_ROUTE_KEY)
  if (!saved || saved === '/' || !saved.startsWith('/')) return
  window.history.replaceState(null, '', saved)
}

restoreLaunchRoute()

if (import.meta.env.PROD) {
  registerSW({ immediate: true })
}

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </StrictMode>
)
