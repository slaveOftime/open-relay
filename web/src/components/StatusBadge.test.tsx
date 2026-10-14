import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'

import StatusBadge from './StatusBadge'
import { badgeVariants } from './ui/badge-variants'

describe('StatusBadge', () => {
  it('renders a single input-needed label', () => {
    const markup = renderToStaticMarkup(<StatusBadge status="running" inputNeeded />)

    expect(markup.match(/Input Needed/g)?.length ?? 0).toBe(1)
    expect(markup).not.toContain('Running')
  })

  it('shows explicit permission state without hiding process lifecycle', () => {
    const markup = renderToStaticMarkup(
      <StatusBadge
        status="running"
        inputNeeded
        programStatus={{ state: 'blocked', kind: 'permission', msg: '<script>bad</script>' }}
      />
    )
    expect(markup).toContain('Running')
    expect(markup).toContain('Permission Required')
    expect(markup).toContain('Agent-reported (OSC 7501)')
    expect(markup).not.toContain('<script>')
  })

  it.each(['idle', 'working', 'done', 'blocked', 'error'] as const)(
    'renders %s reports',
    (state) => {
      const markup = renderToStaticMarkup(
        <StatusBadge status="running" inputNeeded={false} programStatus={{ state, progress: 42 }} />
      )
      expect(markup).toContain('OSC 7501')
      expect(markup.includes('42%')).toBe(state === 'working' || state === 'blocked')
    }
  )

  it('keeps the input-needed badge static while the dot pulses', () => {
    expect(badgeVariants({ variant: 'input-needed' })).not.toContain('animate-pulse')
  })
})
