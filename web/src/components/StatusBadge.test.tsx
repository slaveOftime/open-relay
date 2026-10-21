import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'

import StatusBadge from './StatusBadge'
import { badgeVariants } from './ui/badge-variants'

describe('StatusBadge', () => {
  it('renders a single input-needed label without a program report', () => {
    const markup = renderToStaticMarkup(<StatusBadge status="running" inputNeeded />)

    expect(markup.match(/Input Needed/g)?.length ?? 0).toBe(1)
    expect(markup).toContain('title="Process: Running"')
    expect(markup).not.toContain('<span>Running</span>')
  })

  it('renders only the program report when one exists', () => {
    const markup = renderToStaticMarkup(
      <StatusBadge
        status="running"
        inputNeeded
        programStatus={{ state: 'blocked', kind: 'permission', msg: '<script>bad</script>' }}
      />
    )
    expect(markup).not.toContain('title="Process: Running"')
    expect(markup).not.toContain('Input Needed')
    expect(markup).not.toContain('<span>Running</span>')
    expect(markup).toContain('Permission Required')
    expect(markup).toContain('Agent-reported (OSC 7501)')
    expect(markup).not.toContain('<script>')
  })

  it.each(['idle', 'working', 'done', 'blocked', 'error'] as const)(
    'renders %s reports instead of the lifecycle badge',
    (state) => {
      const markup = renderToStaticMarkup(
        <StatusBadge status="running" inputNeeded={false} programStatus={{ state, progress: 42 }} />
      )
      expect(markup).toContain('OSC 7501')
      expect(markup).not.toContain('title="Process:')
      expect(markup.includes('42%')).toBe(state === 'working' || state === 'blocked')
    }
  )

  it.each(['idle', 'done'] as const)(
    'shows %s as its program label without readiness text',
    (state) => {
      const markup = renderToStaticMarkup(
        <StatusBadge status="running" inputNeeded programStatus={{ state }} />
      )
      expect(markup).toContain(state === 'idle' ? 'Idle' : 'Task Done')
      expect(markup).not.toContain('Ready for Input')
      expect(markup).not.toContain('Input Needed')
      expect(markup).not.toContain('title="Process: Running"')
      expect(markup).not.toContain('<span>Running</span>')
      expect(markup).not.toContain('animate-pulse')
    }
  )

  it.each(['stopping', 'stopped', 'killed', 'failed', 'created'] as const)(
    'shows only the retained report for %s even with a stale attention flag',
    (status) => {
      const markup = renderToStaticMarkup(
        <StatusBadge status={status} inputNeeded programStatus={{ state: 'done' }} />
      )
      expect(markup).not.toContain('Ready for Input')
      expect(markup).not.toContain('Input Needed')
      expect(markup).not.toContain('animate-pulse')
      expect(markup).not.toContain('title="Process:')
      expect(markup).toContain('Task Done')
    }
  )

  it('keeps the input-needed badge static while the dot pulses', () => {
    expect(badgeVariants({ variant: 'input-needed' })).not.toContain('animate-pulse')
  })
})
