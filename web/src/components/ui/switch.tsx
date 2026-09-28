import * as React from 'react'
import * as SwitchPrimitive from '@radix-ui/react-switch'

import { cn } from '@/lib/utils'

type SwitchProps = React.ComponentPropsWithoutRef<typeof SwitchPrimitive.Root> & {
  tone?: 'default' | 'destructive'
}

const Switch = React.forwardRef<React.ElementRef<typeof SwitchPrimitive.Root>, SwitchProps>(
  ({ className, tone = 'default', ...props }, ref) => (
    <SwitchPrimitive.Root
      ref={ref}
      className={cn(
        'inline-flex h-5 w-9 shrink-0 cursor-pointer items-center rounded-full border border-[hsl(var(--border))] transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[hsl(var(--ring))] focus-visible:ring-offset-2 focus-visible:ring-offset-[hsl(var(--background))] disabled:pointer-events-none disabled:opacity-50 data-[state=unchecked]:bg-[hsl(var(--secondary))]',
        tone === 'destructive'
          ? 'data-[state=checked]:border-[hsl(var(--destructive))] data-[state=checked]:bg-[hsl(var(--destructive))]'
          : 'data-[state=checked]:border-[hsl(var(--primary))] data-[state=checked]:bg-[hsl(var(--primary))]',
        className
      )}
      {...props}
    >
      <SwitchPrimitive.Thumb className="pointer-events-none block h-4 w-4 rounded-full bg-[hsl(var(--background))] shadow-sm ring-0 transition-transform data-[state=checked]:translate-x-4 data-[state=unchecked]:translate-x-0.5" />
    </SwitchPrimitive.Root>
  )
)
Switch.displayName = SwitchPrimitive.Root.displayName

export { Switch }
