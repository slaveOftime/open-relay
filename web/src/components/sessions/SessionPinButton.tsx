/**
 * Pin toggle for the sessions list. Pinning is a local, per-browser
 * ordering preference (see `utils/sessionOrdering`), not a server property.
 */

import { Button } from '@/components/ui/button'
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'
import { DrawingPinFilledIcon, DrawingPinIcon } from '@radix-ui/react-icons'

export function SessionPinButton({
  pinned,
  pending,
  onToggle,
}: {
  pinned: boolean
  pending?: boolean
  onToggle: () => void
}) {
  const label = pinned ? 'Unpin session' : 'Pin live session to top'
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          variant={pinned ? 'link' : 'ghost'}
          size="icon"
          aria-label={label}
          disabled={pending}
          onClick={onToggle}
          className="shrink-0"
        >
          {pinned ? (
            <DrawingPinFilledIcon className="h-4 w-4" />
          ) : (
            <DrawingPinIcon className="h-4 w-4" />
          )}
        </Button>
      </TooltipTrigger>
      <TooltipContent>{label}</TooltipContent>
    </Tooltip>
  )
}
