import { useRef, useState } from 'react'
import type { SessionSummary } from '@/api/types'
import { removeSession } from '@/api/client'
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { buttonVariants } from '@/components/ui/button-variants'
import { FormError } from '@/components/ui/form-field'

export default function SessionDeleteConfirmDialog({
  open,
  session,
  node,
  onClose,
  onRemoved,
}: {
  open: boolean
  session: Pick<SessionSummary, 'id' | 'node' | 'status'>
  node?: string
  onClose: () => void
  onRemoved: () => void
}) {
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const inFlight = useRef(false)
  const ownerNode = node ?? session.node
  const force =
    session.status === 'created' || session.status === 'running' || session.status === 'stopping'

  function close() {
    if (inFlight.current) return
    setError(null)
    onClose()
  }

  async function confirmDelete() {
    if (inFlight.current) return
    inFlight.current = true
    setPending(true)
    setError(null)
    try {
      const result = await removeSession(session.id, node ?? session.node ?? undefined, force)
      if (!result.removed) throw new Error('Session was not removed')
      onClose()
      onRemoved()
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : 'Failed to delete session')
    } finally {
      inFlight.current = false
      setPending(false)
    }
  }

  return (
    <AlertDialog
      open={open}
      onOpenChange={(next) => {
        if (!next) close()
      }}
    >
      <AlertDialogContent className="max-w-sm">
        <AlertDialogHeader>
          <AlertDialogTitle>Delete Session</AlertDialogTitle>
          <AlertDialogDescription>
            Permanently delete session{' '}
            <span className="font-mono text-[hsl(var(--foreground))]">
              {session.id.slice(0, 7)}
            </span>
            {ownerNode && (
              <>
                {' '}
                on <span className="font-medium text-[hsl(var(--foreground))]">{ownerNode}</span>
              </>
            )}{' '}
            and its files?
            {force && ' The running process will be killed first.'}
          </AlertDialogDescription>
        </AlertDialogHeader>
        {error && <FormError>{error}</FormError>}
        <AlertDialogFooter>
          <AlertDialogCancel
            disabled={pending}
            className={buttonVariants({ variant: 'ghost', size: 'sm' })}
          >
            Cancel
          </AlertDialogCancel>
          <AlertDialogAction
            disabled={pending}
            className={buttonVariants({ variant: 'destructive', size: 'sm' })}
            onClick={(event) => {
              event.preventDefault()
              void confirmDelete()
            }}
          >
            {pending ? 'Deleting…' : 'Delete session'}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  )
}
