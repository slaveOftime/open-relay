import * as React from 'react'
import { cva, type VariantProps } from 'class-variance-authority'
import { cn } from '@/lib/utils'

const badgeVariants = cva(
  'inline-flex items-center gap-1.5 rounded-full border px-2.5 py-0.5 text-xs font-semibold transition-colors',
  {
    variants: {
      variant: {
        default:
          'border-transparent bg-[hsl(var(--primary))] text-[hsl(var(--primary-foreground))]',
        secondary:
          'border-transparent bg-[hsl(var(--secondary))] text-[hsl(var(--secondary-foreground))]',
        destructive:
          'bg-red-50 text-red-700 border-red-200 dark:bg-red-900/50 dark:text-red-300 dark:border-red-800/40',
        outline: 'border-[hsl(var(--border))] text-[hsl(var(--foreground))]',
        running:
          'border-green-200 bg-green-50 text-green-700 dark:border-green-800/40 dark:bg-green-900/30 dark:text-green-300',
        stopping:
          'border-yellow-200 bg-yellow-50 text-yellow-700 dark:border-yellow-800/40 dark:bg-yellow-900/30 dark:text-yellow-300',
        stopped:
          'border-gray-300 bg-gray-100 text-gray-600 dark:border-gray-700/50 dark:bg-gray-800/50 dark:text-gray-400',
        killed:
          'border-orange-200 bg-orange-50 text-orange-700 dark:border-orange-800/40 dark:bg-orange-900/30 dark:text-orange-300',
        failed:
          'border-red-200 bg-red-50 text-red-700 dark:border-red-800/40 dark:bg-red-900/30 dark:text-red-300',
        created:
          'border-blue-200 bg-blue-50 text-blue-700 dark:border-blue-800/40 dark:bg-blue-900/30 dark:text-blue-300',
        'input-needed':
          'border-amber-200 bg-amber-50 text-amber-700 animate-pulse dark:border-amber-700/60 dark:bg-amber-900/30 dark:text-amber-300',
        attached:
          'border-indigo-200 bg-indigo-50 text-indigo-700 dark:border-indigo-700/50 dark:bg-indigo-900/30 dark:text-indigo-300',
      },
    },
    defaultVariants: {
      variant: 'default',
    },
  }
)

export interface BadgeProps
  extends React.HTMLAttributes<HTMLDivElement>, VariantProps<typeof badgeVariants> {}

function Badge({ className, variant, ...props }: BadgeProps) {
  return <div className={cn(badgeVariants({ variant }), className)} {...props} />
}

export { Badge, badgeVariants }
