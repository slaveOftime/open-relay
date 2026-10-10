import * as React from 'react'
import { Cross2Icon } from '@radix-ui/react-icons'

import { cn } from '@/utils/cn'

import { Input, type InputProps } from './input'

export interface ClearableInputProps extends InputProps {
  /** Sizes the wrapper around the input, for the cases that need it (flex rows). */
  containerClassName?: string
  /** Accessible name for the clear button. */
  clearLabel?: string
}

/**
 * An Input that gets a clear button once it has text.
 *
 * The touch target is the point of this primitive: a bare icon inside an input
 * measures ~14px, which is miserable to hit on a phone. The button measures
 * 32px and its hit area is stretched to 44px with a pseudo-element, so the
 * control stays visually small inside the field's 32px height without the
 * cramped target.
 *
 * Controlled use only — the button shows while `value` is a non-empty string.
 * Clearing dispatches an empty-value change through the field's own `onChange`,
 * so whatever else that handler does (resetting a page, bumping a version)
 * still happens. A `FormField` wrapping this component is fine: Radix's
 * `Form.Control asChild` props and ref reach the inner input.
 */
const ClearableInput = React.forwardRef<HTMLInputElement, ClearableInputProps>(
  (
    { className, containerClassName, clearLabel = 'Clear', value, onChange, disabled, ...props },
    ref
  ) => {
    const hasValue = typeof value === 'string' && value.length > 0
    const showClear = hasValue && !disabled && !props.readOnly

    const clear = () => {
      // A synthesized empty-value change, so the field's own handler clears
      // it. Both target and currentTarget carry the value: handlers read
      // either.
      const cleared = {
        target: { value: '' },
        currentTarget: { value: '' },
      } as unknown as React.ChangeEvent<HTMLInputElement>
      onChange?.(cleared)
    }

    return (
      <div className={cn('relative w-full', containerClassName)}>
        <Input
          ref={ref}
          className={cn(showClear && 'pr-9', className)}
          value={value}
          onChange={onChange}
          disabled={disabled}
          {...props}
        />
        {showClear && (
          <button
            type="button"
            aria-label={clearLabel}
            // Keep focus in the field: a clear button that blurs the input
            // first breaks fields that close on blur, and costs a tap to
            // resume typing.
            onPointerDown={(event) => event.preventDefault()}
            onClick={clear}
            className={cn(
              'absolute right-1 top-1/2 flex h-8 w-8 -translate-y-1/2 items-center justify-center',
              'rounded-full text-[hsl(var(--muted-foreground))] transition-colors',
              'hover:bg-[hsl(var(--accent))] hover:text-[hsl(var(--foreground))]',
              'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[hsl(var(--ring))] focus-visible:ring-offset-1',
              // Stretches the hit area to 44px without growing the visible
              // button past the field's height.
              "after:absolute after:-inset-1.5 after:content-['']"
            )}
          >
            <Cross2Icon className="h-3.5 w-3.5" />
          </button>
        )}
      </div>
    )
  }
)
ClearableInput.displayName = 'ClearableInput'

export { ClearableInput }
