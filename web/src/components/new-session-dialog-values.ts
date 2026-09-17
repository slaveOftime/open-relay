import type { SessionSummary } from '@/api/types'
import { formatSessionTagInput } from '@/lib/sessionMetadata'

export type NewSessionInitialValues = {
  cmd: string
  args: string
  title: string
  tags: string
  cwd: string
  notifications_enabled: boolean
}

export function buildNewSessionInitialValues(
  session: Pick<SessionSummary, 'command' | 'args' | 'title' | 'tags' | 'cwd' | 'notifications_enabled'>
): NewSessionInitialValues {
  return {
    cmd: session.command,
    args: session.args
      .map((arg) => (/\s/.test(arg) ? `"${arg.replace(/"/g, '\\"')}"` : arg))
      .join(' '),
    title: session.title ?? '',
    tags: formatSessionTagInput(session.tags),
    cwd: session.cwd ?? '',
    notifications_enabled: session.notifications_enabled
  }
}
