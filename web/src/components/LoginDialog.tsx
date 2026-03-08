import { useCallback, useEffect, useRef, useState } from 'react'
import { login, setToken } from '../api/client'
import { TooManyAttemptsError } from '../api/types'
import { Button } from './ui/button'
import { Input } from './ui/input'

interface LoginDialogProps {
  /** When true the dialog is shown; cannot be closed by the user. */
  open: boolean
  /** Called after a successful login with the issued token. */
  onSuccess: (token: string) => void
}

export default function LoginDialog({ open, onSuccess }: LoginDialogProps) {
  const [password, setPassword] = useState('')
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [attemptsRemaining, setAttemptsRemaining] = useState<number | null>(null)
  const [lockedUntil, setLockedUntil] = useState<Date | null>(null)
  const [countdown, setCountdown] = useState<string>('')
  const inputRef = useRef<HTMLInputElement>(null)

  // Focus the password field whenever the dialog opens.
  useEffect(() => {
    if (open) {
      setPassword('')
      setError(null)
      setAttemptsRemaining(null)
      setTimeout(() => inputRef.current?.focus(), 50)
    }
  }, [open])

  // Live countdown when locked out.
  useEffect(() => {
    if (!lockedUntil) return
    const update = () => {
      const remaining = lockedUntil.getTime() - Date.now()
      if (remaining <= 0) {
        setLockedUntil(null)
        setCountdown('')
        setError(null)
        return
      }
      const m = Math.floor(remaining / 60_000)
      const s = Math.floor((remaining % 60_000) / 1000)
      setCountdown(`${m}m ${String(s).padStart(2, '0')}s`)
    }
    update()
    const id = setInterval(update, 1000)
    return () => clearInterval(id)
  }, [lockedUntil])

  const handleSubmit = useCallback(
    async (e: React.FormEvent) => {
      e.preventDefault()
      if (loading || lockedUntil) return
      setLoading(true)
      setError(null)
      try {
        const resp = await login(password)
        setToken(resp.token)
        onSuccess(resp.token)
      } catch (err) {
        if (err instanceof TooManyAttemptsError) {
          const until = new Date(Date.now() + err.retryAfterSeconds * 1000)
          setLockedUntil(until)
          setAttemptsRemaining(0)
          setError('Too many failed attempts.')
        } else {
          // err may carry attemptsRemaining
          const e = err as Error & { attemptsRemaining?: number }
          if (e.attemptsRemaining != null) {
            setAttemptsRemaining(e.attemptsRemaining)
            setError(
              `Incorrect password. ${e.attemptsRemaining} attempt${e.attemptsRemaining === 1 ? '' : 's'} remaining.`
            )
          } else {
            setError('Incorrect password.')
          }
          setPassword('')
          inputRef.current?.focus()
        }
      } finally {
        setLoading(false)
      }
    },
    [password, loading, lockedUntil, onSuccess]
  )

  if (!open) return null

  const isLocked = !!lockedUntil

  return (
    /* Full-viewport blocking overlay — intentionally non-dismissible */
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/70 backdrop-blur-sm">
      <div className="w-full max-w-sm rounded-xl border border-white/10 bg-[hsl(var(--background))] p-8 shadow-2xl">
        {/* Logo / title */}
        <div className="mb-6 text-center">
          <div className="mb-3 flex justify-center">
            <img src="/icon.svg" alt="" aria-hidden="true" className="h-16 w-16" />
          </div>
          <h1 className="text-xl font-semibold tracking-tight">Open Relay</h1>
          <p className="mt-1 text-sm text-muted-foreground">
            Enter the daemon password to continue
          </p>
        </div>

        <form onSubmit={handleSubmit} className="space-y-4">
          <Input
            ref={inputRef}
            type="password"
            placeholder="Password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            disabled={loading || isLocked}
            autoFocus
            autoComplete="current-password"
          />

          {error && (
            <p className="text-sm text-destructive">
              {error}
              {isLocked && countdown && (
                <span className="ml-1 font-mono">Retry in {countdown}.</span>
              )}
            </p>
          )}

          {attemptsRemaining != null && !isLocked && attemptsRemaining > 0 && (
            <p className="text-xs text-muted-foreground">
              {attemptsRemaining} attempt{attemptsRemaining === 1 ? '' : 's'} remaining before
              lockout.
            </p>
          )}

          <Button type="submit" className="w-full" disabled={loading || isLocked || !password}>
            {loading ? 'Verifying…' : 'Sign in'}
          </Button>
        </form>
      </div>
    </div>
  )
}
