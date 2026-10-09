/**
 * Bell toggle for per-session push notifications.
 *
 * Notifications cannot be enabled after a session exits, so the button
 * reports that reason rather than doing nothing silently.
 */

import { Button } from '@/components/ui/button'
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'
import { BellIcon } from '@radix-ui/react-icons'

export function SessionNotificationButton({
  enabled,
  disabled,
  pending,
  onToggle,
}: {
  enabled: boolean
  disabled?: boolean
  pending?: boolean
  onToggle: () => void
}) {
  const label = disabled
    ? 'Notifications unavailable after session exit'
    : enabled
      ? 'Turn notifications off'
      : 'Turn notifications on'

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Button
          variant="ghost"
          size="icon"
          aria-label={label}
          disabled={disabled || pending}
          onClick={onToggle}
          className="shrink-0"
        >
          <span className="relative inline-flex h-4 w-4 items-center justify-center">
            <BellIcon className="h-4 w-4" />
            {!enabled && (
              <span className="absolute h-[1.5px] w-5 -rotate-45 rounded-full bg-current" />
            )}
          </span>
        </Button>
      </TooltipTrigger>
      <TooltipContent>{pending ? 'Updating notifications…' : label}</TooltipContent>
    </Tooltip>
  )
}
