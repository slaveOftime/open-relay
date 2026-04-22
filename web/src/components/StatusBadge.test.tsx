import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'

import StatusBadge from './StatusBadge'
import { badgeVariants } from './ui/badge'

describe('StatusBadge', () => {
  it('renders a single input-needed label', () => {
    const markup = renderToStaticMarkup(<StatusBadge status="running" inputNeeded />)

    expect(markup.match(/Input Needed/g)?.length ?? 0).toBe(1)
    expect(markup).not.toContain('Running')
  })

  it('keeps the input-needed badge static while the dot pulses', () => {
    expect(badgeVariants({ variant: 'input-needed' })).not.toContain('animate-pulse')
  })
})
