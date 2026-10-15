import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'

import StatusBadge from './StatusBadge'
import { badgeVariants } from './ui/badge-variants'

describe('StatusBadge', () => {
  it('renders a single input-needed label', () => {
    const markup = renderToStaticMarkup(<StatusBadge status="running" inputNeeded />)

    expect(markup.match(/Input Needed/g)?.length ?? 0).toBe(1)
    expect(markup).not.toContain('<span>Running</span>')
  })

  it('shows input-needed alongside permission detail and lifecycle tooltip', () => {
    const markup = renderToStaticMarkup(
      <StatusBadge
        status="running"
        inputNeeded
        programStatus={{ state: 'blocked', kind: 'permission', msg: '<script>bad</script>' }}
      />
    )
    expect(markup).toContain('title="Process: Running"')
    expect(markup).toContain('Input Needed')
    expect(markup).not.toContain('<span>Running</span>')
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

  it.each(['idle', 'done'] as const)('shows %s as ready without a busy pulse', (state) => {
    const markup = renderToStaticMarkup(
      <StatusBadge status="running" inputNeeded programStatus={{ state }} />
    )
    expect(markup).toContain('Ready for Input')
    expect(markup).toContain('title="Process: Running"')
    expect(markup).toContain(state === 'idle' ? 'Idle' : 'Task Done')
    expect(markup).not.toContain('<span>Running</span>')
    expect(markup).not.toContain('animate-pulse')
  })

  it.each(['stopping', 'stopped', 'killed', 'failed', 'created'] as const)(
    'does not show %s as ready even with a stale attention flag',
    (status) => {
      const markup = renderToStaticMarkup(
        <StatusBadge status={status} inputNeeded programStatus={{ state: 'done' }} />
      )
      expect(markup).not.toContain('Ready for Input')
      expect(markup).not.toContain('Input Needed')
      expect(markup).not.toContain('animate-pulse')
      expect(markup).toContain('Task Done')
    }
  )

  it('keeps the input-needed badge static while the dot pulses', () => {
    expect(badgeVariants({ variant: 'input-needed' })).not.toContain('animate-pulse')
  })
})
