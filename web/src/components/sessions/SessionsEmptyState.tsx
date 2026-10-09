/**
 * What the list shows when a node has no sessions at all.
 */

import Logo from '@/components/Logo'
import { Button } from '@/components/ui/button'
import { PlusIcon } from '@radix-ui/react-icons'

export function SessionsEmptyState({
  onNewSession,
  selectedNode,
}: {
  onNewSession: () => void
  selectedNode: string | null
}) {
  return (
    <div className="flex flex-col items-center justify-center py-24 text-[hsl(var(--muted-foreground))] gap-3">
      <Logo size={80} />
      <p className="text-sm text-[hsl(var(--muted-foreground))]">
        No sessions yet{selectedNode ? ` on ${selectedNode}` : ''}.
      </p>
      <Button size="sm" onClick={onNewSession}>
        <PlusIcon className="w-4 h-4" />
        New Session
      </Button>
    </div>
  )
}
