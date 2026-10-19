import { CircleHelp } from 'lucide-react'

import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'
import { cn } from '@/utils/cn'

interface Props {
  /** Tooltip placement; the attach composer sits near the bottom of a drawer. */
  side?: 'top' | 'right' | 'bottom' | 'left'
  className?: string
}

/**
 * Inline help button for the key composer's conventions.
 *
 * The rules live in `splitKeyInput` / `parseKeyInputSpecs` (`@/utils/key-input`)
 * but are invisible in the UI: nothing hints that a second space is a Space key
 * while the first only separates, or which words are named keys. This button
 * carries those rules in a tooltip so both the attach panel composer and the
 * quick-key editor explain themselves.
 *
 * It is deliberately no taller than the field it annotates, so the surrounding
 * row keeps its height and the explanation arrives as a popup rather than
 * pushing layout around.
 */
export default function KeyInputRulesHint({ side = 'top', className }: Props) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          aria-label="Key input rules"
          className={cn(
            'inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-md text-[hsl(var(--muted-foreground))] transition-colors',
            'hover:bg-[hsl(var(--accent))] hover:text-[hsl(var(--foreground))]',
            'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[hsl(var(--ring))] focus-visible:ring-offset-1',
            className
          )}
        >
          <CircleHelp className="h-3.5 w-3.5" aria-hidden="true" />
        </button>
      </TooltipTrigger>
      <TooltipContent
        side={side}
        align="start"
        className="max-w-64 space-y-1.5 text-left leading-relaxed"
      >
        <p className="font-medium">Key input rules</p>
        <p>
          Keys are split on spaces. One space only separates them; each extra space sends a Space:{' '}
          <code className="whitespace-pre rounded bg-[hsl(var(--muted))] px-1">{'a  b'}</code> sends{' '}
          <code>a</code>, Space, <code>b</code>.
        </p>
        <p>
          Named keys (enter, tab, esc, space, arrows, home/end, pgup/pgdn, del, ins) and combos
          (ctrl+c, alt+x, shift+tab) send one keystroke. Any other word is typed out letter by
          letter.
        </p>
      </TooltipContent>
    </Tooltip>
  )
}
