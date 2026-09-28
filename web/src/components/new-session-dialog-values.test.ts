import { describe, expect, it } from 'vitest'
import { buildNewSessionInitialValues } from './new-session-dialog-values'

describe('buildNewSessionInitialValues', () => {
  const source = {
    id: 'source-1',
    node: 'worker-a',
    command: 'bash',
    args: ['-c', 'echo hello'],
    title: 'Original',
    tags: ['work'],
    cwd: '/tmp',
    notifications_enabled: true,
  }

  it('carries the original session and owning node for optional removal', () => {
    const values = buildNewSessionInitialValues(source)
    expect(values.sourceSession).toEqual({ id: 'source-1', node: 'worker-a' })
    expect(values.cmd).toBe('bash')
    expect(values.args).toBe('-c "echo hello"')
  })

  it('keeps local source ownership distinct from a remote target node', () => {
    expect(buildNewSessionInitialValues({ ...source, node: null }).sourceSession).toEqual({
      id: 'source-1',
      node: null,
    })
  })
})
