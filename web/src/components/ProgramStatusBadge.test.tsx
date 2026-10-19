import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'

import type { ProgramStatus } from '@/api/types'
import ProgramStatusBadge from './ProgramStatusBadge'
import { badgeVariants } from './ui/badge-variants'

const states = [
  { state: 'idle', label: 'Idle' },
  { state: 'working', label: 'Working' },
  { state: 'done', label: 'Task Done' },
  { state: 'blocked', label: 'Input Required' },
  { state: 'error', label: 'Task Error' },
] as const

describe('ProgramStatusBadge', () => {
  it.each(states)(
    'uses the program-$state variant with readable medium text',
    ({ state, label }) => {
      const markup = renderToStaticMarkup(<ProgramStatusBadge report={{ state }} />)
      const classes = markup.match(/class="([^"]+)"/)?.[1].split(/\s+/) ?? []
      const tokenClasses = [
        `border-[hsl(var(--program-status-${state}))]/30`,
        `bg-[hsl(var(--program-status-${state}))]/12`,
        `text-[hsl(var(--program-status-${state}-foreground))]`,
      ]

      for (const tokenClass of tokenClasses) {
        expect(badgeVariants({ variant: `program-${state}` })).toContain(tokenClass)
      }
      expect(classes).toEqual(
        expect.arrayContaining([
          ...tokenClasses,
          'font-medium',
          'text-xs',
          'tabular-nums',
          'max-w-full',
          'rounded-full',
          'px-2',
          // One-line pill, like every other Badge: `max-w-full` clamps the box
          // and the status cell/card header own the width, so the label must
          // never wrap the badge into a taller shape.
          'whitespace-nowrap',
        ])
      )
      expect(classes).not.toContain('font-light')
      expect(classes).not.toContain('font-semibold')
      expect(markup).toContain(`>${label}</div>`)
    }
  )

  it.each(states)('only displays $state progress when working or blocked', ({ state, label }) => {
    for (const progress of [0, 42, 100]) {
      const markup = renderToStaticMarkup(<ProgramStatusBadge report={{ state, progress }} />)
      const suffix = state === 'working' || state === 'blocked' ? ` ${progress}%` : ''
      expect(markup).toContain(`>${label}${suffix}</div>`)
      expect(markup.includes('%')).toBe(state === 'working' || state === 'blocked')
    }
  })

  it.each(['working', 'blocked'] as const)('omits absent %s progress', (state) => {
    for (const progress of [undefined, null]) {
      const markup = renderToStaticMarkup(<ProgramStatusBadge report={{ state, progress }} />)
      expect(markup).not.toContain('%')
    }
  })

  it.each([
    { kind: 'permission', label: 'Permission Required' },
    { kind: 'question', label: 'Answer Required' },
    { kind: 'auth', label: 'Authentication Required' },
    { kind: null, label: 'Input Required' },
    { kind: undefined, label: 'Input Required' },
  ] as const)('preserves the blocked $kind label and progress', ({ kind, label }) => {
    const markup = renderToStaticMarkup(
      <ProgramStatusBadge report={{ state: 'blocked', kind, progress: 42 }} />
    )
    expect(markup).toContain(`>${label} 42%</div>`)
  })

  it.each(states.filter(({ state }) => state !== 'blocked'))(
    'ignores blocked kinds for $state reports',
    ({ state, label }) => {
      const markup = renderToStaticMarkup(
        <ProgramStatusBadge report={{ state, kind: 'permission' }} />
      )
      expect(markup).toContain(`>${label}</div>`)
      expect(markup).not.toContain('Permission Required')
    }
  )

  it('keeps tooltip detail order and escapes untrusted report fields', () => {
    const report: ProgramStatus = {
      state: 'working',
      app: 'helper & "agent"',
      id: '<task>',
      title: 'Review & verify',
      msg: '<script>alert("no")</script>',
    }
    const markup = renderToStaticMarkup(<ProgramStatusBadge report={report} />)

    expect(markup).toContain(
      'title="Agent-reported (OSC 7501): helper &amp; &quot;agent&quot; · &lt;task&gt; · Review &amp; verify · &lt;script&gt;alert(&quot;no&quot;)&lt;/script&gt;"'
    )
    expect(markup).not.toContain('<task>')
    expect(markup).not.toContain('<script>')
    expect(markup).toContain('>Working</div>')
  })

  it('omits empty tooltip fields without adding separators', () => {
    const markup = renderToStaticMarkup(
      <ProgramStatusBadge report={{ state: 'idle', app: '', id: null, title: 'Review', msg: '' }} />
    )
    expect(markup).toContain('title="Agent-reported (OSC 7501): Review"')
    expect(markup).not.toContain(' · ')
  })

  it('keeps the plain agent-reported tooltip when there is no detail', () => {
    const markup = renderToStaticMarkup(<ProgramStatusBadge report={{ state: 'idle' }} />)
    expect(markup).toContain('title="Agent-reported (OSC 7501)"')
  })
})
