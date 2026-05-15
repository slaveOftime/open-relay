import type { SessionSummary } from '@/api/types'
import { formatSessionTagInput } from '@/lib/sessionMetadata'

export type NewSessionInitialValues = {
  cmd: string
  args: string
  title: string
  tags: string
  cwd: string
}

export function buildNewSessionInitialValues(
  session: Pick<SessionSummary, 'command' | 'args' | 'title' | 'tags' | 'cwd'>
): NewSessionInitialValues {
  return {
    cmd: session.command,
    args: session.args
      .map((arg) => (/\s/.test(arg) ? `"${arg.replace(/"/g, '\\"')}"` : arg))
      .join(' '),
    title: session.title ?? '',
    tags: formatSessionTagInput(session.tags),
    cwd: session.cwd ?? '',
  }
}
