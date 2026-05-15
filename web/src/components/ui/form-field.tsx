import * as React from 'react'
import * as Form from '@radix-ui/react-form'

import { cn } from '@/lib/utils'

type FormFieldProps = {
  name: string
  label: React.ReactNode
  required?: boolean
  description?: React.ReactNode
  error?: React.ReactNode
  className?: string
  children: React.ReactNode
}

function FormField({
  name,
  label,
  required = false,
  description,
  error,
  className,
  children,
}: FormFieldProps) {
  return (
    <Form.Field name={name} className={cn('flex flex-col gap-1.5', className)}>
      <Form.Label className="text-xs text-[hsl(var(--muted-foreground))]">
        {label}
        {required && <span className="ml-1 text-[hsl(var(--destructive))]">*</span>}
      </Form.Label>
      <Form.Control asChild>{children}</Form.Control>
      {description ? <FormDescription>{description}</FormDescription> : null}
      {error ? <FormError>{error}</FormError> : null}
    </Form.Field>
  )
}

function FormDescription({
  className,
  ...props
}: React.HTMLAttributes<HTMLParagraphElement>) {
  return (
    <p
      className={cn('text-[11px] text-[hsl(var(--muted-foreground))]', className)}
      {...props}
    />
  )
}

function FormError({ className, ...props }: React.HTMLAttributes<HTMLParagraphElement>) {
  return (
    <p className={cn('text-xs text-[hsl(var(--destructive))]', className)} role="alert" {...props} />
  )
}

function FormActions({ className, ...props }: React.HTMLAttributes<HTMLDivElement>) {
  return <div className={cn('flex justify-end gap-2 pt-1', className)} {...props} />
}

export { FormActions, FormDescription, FormError, FormField }
