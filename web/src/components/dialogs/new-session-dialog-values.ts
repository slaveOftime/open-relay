import type { SessionSummary } from '@/api/types'
import { formatSessionTagInput } from '@/lib/session-metadata'

export type NewSessionInitialValues = {
  cmd: string
  args: string
  title: string
  tags: string
  cwd: string
  notifications_enabled: boolean
  /** Best-effort resume command advertised by the source session, if any. */
  resumeCommand: string | null
  sourceSession: { id: string; node: string | null }
}

export function buildNewSessionInitialValues(
  session: Pick<
    SessionSummary,
    | 'id'
    | 'node'
    | 'command'
    | 'args'
    | 'title'
    | 'tags'
    | 'cwd'
    | 'notifications_enabled'
    | 'resume_command'
  >
): NewSessionInitialValues {
  return {
    cmd: session.command,
    args: session.args
      .map((arg) => (/\s/.test(arg) ? `"${arg.replace(/"/g, '\\"')}"` : arg))
      .join(' '),
    title: session.title ?? '',
    tags: formatSessionTagInput(session.tags),
    cwd: session.cwd ?? '',
    notifications_enabled: session.notifications_enabled,
    resumeCommand: session.resume_command?.trim() || null,
    sourceSession: { id: session.id, node: session.node ?? null },
  }
}
