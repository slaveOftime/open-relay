/**
 * The mobile-only key bar below the attach textarea: arrow keys, tab, esc,
 * enter, and the drawer toggle.
 *
 * Every key is a hold-to-repeat button. They park DOM focus on the button
 * itself rather than moving it to xterm, and suppress the focus ring, because
 * on touch a pressed key is not a keyboard-focused control and a ring there
 * is noise (see `holdRepeatProps`).
 */

import {
  ChevronDownIcon,
  ChevronLeftIcon,
  ChevronRightIcon,
  ChevronUpIcon,
  DoubleArrowDownIcon,
  DoubleArrowUpIcon,
} from '@radix-ui/react-icons'

import { Button } from '@/components/ui/button'
import { holdRepeatProps, type RepeatControls } from '@/hooks/use-repeat-while-pressed'

const HOLD_KEY_FOCUS_NONE =
  'focus:outline-none focus:ring-0 focus:ring-offset-0 focus-visible:outline-none focus-visible:ring-0 focus-visible:ring-offset-0'

const NAV_KEY_CLASS = `${HOLD_KEY_FOCUS_NONE} shrink-0 select-none text-[hsl(var(--primary))] px-2.5`
// Esc and Enter are destructive/confirmatory, so they read in the warning color
// rather than the accent used for navigation.
const ACTION_KEY_CLASS = `${HOLD_KEY_FOCUS_NONE} shrink-0 select-none text-amber-600 px-2.5`

interface MobileKeyBarProps {
  repeat: RepeatControls
  onSendKeySpec: (spec: string) => void
  drawerOpen: boolean
  onToggleDrawer: () => void
}

export function MobileKeyBar({
  repeat,
  onSendKeySpec,
  drawerOpen,
  onToggleDrawer,
}: MobileKeyBarProps) {
  return (
    <div className="sm:hidden w-full h-10 flex items-center gap-1 justify-between overflow-hidden">
      <div className="sm:hidden w-full h-10 flex items-center overflow-y-hidden overflow-x-auto select-none">
        <Button
          type="button"
          variant={'ghost'}
          className={NAV_KEY_CLASS}
          {...holdRepeatProps(repeat, () => onSendKeySpec('left'))}
          aria-label="Left"
        >
          <ChevronLeftIcon className="w-6 h-6" />
        </Button>
        <Button
          type="button"
          variant={'ghost'}
          className={NAV_KEY_CLASS}
          {...holdRepeatProps(repeat, () => onSendKeySpec('up'))}
          aria-label="Up"
        >
          <ChevronUpIcon className="w-6 h-6" />
        </Button>
        <Button
          type="button"
          variant={'ghost'}
          className={NAV_KEY_CLASS}
          {...holdRepeatProps(repeat, () => onSendKeySpec('down'))}
          aria-label="Down"
        >
          <ChevronDownIcon className="w-6 h-6" />
        </Button>
        <Button
          type="button"
          variant={'ghost'}
          className={NAV_KEY_CLASS}
          {...holdRepeatProps(repeat, () => onSendKeySpec('right'))}
          aria-label="Right"
        >
          <ChevronRightIcon className="w-6 h-6" />
        </Button>
        <Button
          type="button"
          variant={'ghost'}
          className={NAV_KEY_CLASS}
          {...holdRepeatProps(repeat, () => onSendKeySpec('tab'))}
          aria-label="Tab"
        >
          Tab
        </Button>
        <Button
          type="button"
          variant={'ghost'}
          className={ACTION_KEY_CLASS}
          {...holdRepeatProps(repeat, () => onSendKeySpec('esc'))}
          aria-label="Esc"
        >
          Esc
        </Button>
        <Button
          type="button"
          variant={'ghost'}
          className={ACTION_KEY_CLASS}
          {...holdRepeatProps(repeat, () => onSendKeySpec('enter'))}
          aria-label="Enter"
        >
          Enter
        </Button>
      </div>
      <Button
        variant="ghost"
        className="shrink-0 px-2.5"
        onClick={onToggleDrawer}
        aria-label="Open input panel"
      >
        {drawerOpen ? (
          <DoubleArrowDownIcon className="w-5 h-5" />
        ) : (
          <DoubleArrowUpIcon className="w-5 h-5" />
        )}
      </Button>
    </div>
  )
}
