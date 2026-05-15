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
import { buttonVariants } from '@/components/ui/button'
import { cn } from '@/lib/utils'

type SessionAction = 'stop' | 'kill'

const SESSION_ACTION_COPY: Record<
  SessionAction,
  { title: string; confirmLabel: string; description: string }
> = {
  stop: {
    title: 'Stop Session',
    confirmLabel: 'Stop session',
    description: 'A graceful shutdown signal will be sent.',
  },
  kill: {
    title: 'Kill Session',
    confirmLabel: 'Kill session',
    description: 'The process will be terminated immediately.',
  },
}

interface SessionActionConfirmDialogProps {
  action: SessionAction | null
  sessionId: string
  onConfirm: (action: SessionAction) => void
  onClose: () => void
}

export default function SessionActionConfirmDialog({
  action,
  sessionId,
  onConfirm,
  onClose,
}: SessionActionConfirmDialogProps) {
  const copy = action ? SESSION_ACTION_COPY[action] : null

  return (
    <AlertDialog
      open={action !== null}
      onOpenChange={(open) => {
        if (!open) onClose()
      }}
    >
      {copy ? (
        <AlertDialogContent className="max-w-sm">
          <AlertDialogHeader>
            <AlertDialogTitle>{copy.title}</AlertDialogTitle>
            <AlertDialogDescription>
              Are you sure you want to <span className="font-medium text-[hsl(var(--foreground))]">{action}</span>{' '}
              session <span className="font-mono text-[hsl(var(--foreground))]">{sessionId.slice(0, 7)}</span>?{' '}
              {copy.description}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel className={buttonVariants({ variant: 'ghost', size: 'sm' })}>
              Cancel
            </AlertDialogCancel>
            <AlertDialogAction
              className={cn(buttonVariants({ variant: action, size: 'sm' }))}
              onClick={() => {
                if (action) onConfirm(action)
              }}
            >
              {copy.confirmLabel}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      ) : null}
    </AlertDialog>
  )
}
