import { Suspense, lazy, useCallback, useEffect, useState } from 'react'
import { BrowserRouter, Routes, Route, Navigate, useLocation, useNavigate } from 'react-router-dom'
import LoginDialog from './components/LoginDialog'
import { getAuthStatus, getToken } from './api/client'
import { notificationClickMessageTarget } from '@/lib/notifications'

const SessionsPage = lazy(() => import('./pages/SessionsPage'))
const SessionDetailPage = lazy(() => import('./pages/SessionDetailPage'))

const LAST_ROUTE_KEY = 'open-relay:last-route'

function LastRoutePersistence() {
  const location = useLocation()

  useEffect(() => {
    const current = `${location.pathname}${location.search}${location.hash}`
    localStorage.setItem(LAST_ROUTE_KEY, current)
  }, [location.pathname, location.search, location.hash])

  return null
}

function NotificationClickRouting() {
  const navigate = useNavigate()

  useEffect(() => {
    if (!('serviceWorker' in navigator)) return

    function onMessage(event: MessageEvent) {
      const target = notificationClickMessageTarget(event.data, window.location.origin)
      if (!target) return
      navigate(target)
      window.focus()
    }

    navigator.serviceWorker.addEventListener('message', onMessage)
    return () => navigator.serviceWorker.removeEventListener('message', onMessage)
  }, [navigate])

  return null
}

export default function App() {
  // null = still loading, false = no auth required, true = auth required
  const [authRequired, setAuthRequired] = useState<boolean | null>(null)
  const [isAuthed, setIsAuthed] = useState(false)

  useEffect(() => {
    getAuthStatus()
      .then(({ auth_required }) => {
        setAuthRequired(auth_required)
        if (!auth_required) {
          setIsAuthed(true)
        } else {
          // If a token is already in sessionStorage, trust it until the first
          // 401 response (the interceptor in req() will clear it + re-trigger).
          if (getToken()) setIsAuthed(true)
        }
      })
      .catch(() => {
        // If we can't even reach /api/auth/status, show login (daemon may be starting)
        setAuthRequired(true)
      })
  }, [])

  const handleLoginSuccess = useCallback(() => {
    setIsAuthed(true)
  }, [])

  // Re-authenticate when any fetch triggers a 401.
  useEffect(() => {
    function onAuthRequired(e: Event) {
      if (e instanceof CustomEvent && e.type === 'oly:auth-required') {
        setIsAuthed(false)
      }
    }
    window.addEventListener('oly:auth-required', onAuthRequired)
    return () => window.removeEventListener('oly:auth-required', onAuthRequired)
  }, [])

  const showLogin = authRequired === true && !isAuthed

  return (
    <BrowserRouter>
      <LastRoutePersistence />
      <NotificationClickRouting />
      <LoginDialog open={showLogin} onSuccess={handleLoginSuccess} />

      {/* Render the app; when auth is required but not yet granted, the login
          dialog is shown on top and the app underneath is inert. */}
      {authRequired !== null && (
        <Suspense fallback={null}>
          <Routes>
            <Route path="/" element={<SessionsPage />} />
            <Route path="/session/:id" element={<SessionDetailPage />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Routes>
        </Suspense>
      )}
    </BrowserRouter>
  )
}
