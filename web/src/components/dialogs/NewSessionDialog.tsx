import { useEffect, useRef, useState } from 'react'
import * as Form from '@radix-ui/react-form'
import { forceRemoveSession, startSession } from '@/api/client'
import { parseSessionTagInput } from '@/lib/session-metadata'
import { cn } from '@/utils/cn'
import { parseArgString } from '@/utils/format'
import { Button } from '@/components/ui/button'
import { ClearableInput } from '@/components/ui/clearable-input'
import { Switch } from '@/components/ui/switch'
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { FormActions, FormError, FormField } from '@/components/ui/form-field'
import NotificationToggle from '@/components/NotificationToggle'
import type { NewSessionInitialValues } from './new-session-dialog-values'

function splitResumeCommand(command: string): { cmd: string; args: string } {
  const match = command.match(/^(\S+)(?:\s+([\s\S]*))?$/)
  return { cmd: match?.[1] ?? command, args: (match?.[2] ?? '').trim() }
}

export default function NewSessionDialog({
  open,
  onClose,
  onRemovedOriginal,
  initialValues,
  node,
}: {
  open: boolean
  onClose: () => void
  onRemovedOriginal?: () => void
  initialValues?: NewSessionInitialValues
  node?: string
}) {
  const [cmd, setCmd] = useState('')
  const [args, setArgs] = useState('')
  const [title, setTitle] = useState('')
  const [tags, setTags] = useState('')
  const [cwd, setCwd] = useState('')
  const [notificationsEnabled, setNotificationsEnabled] = useState(true)
  const [removeOriginal, setRemoveOriginal] = useState(false)
  const [startedSessionId, setStartedSessionId] = useState<string | null>(null)
  const [creationUncertain, setCreationUncertain] = useState(false)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [resumeApplied, setResumeApplied] = useState(false)
  const resumeRestoreRef = useRef<{ cmd: string; args: string } | null>(null)
  const wasOpenRef = useRef(false)
  const submitPendingRef = useRef(false)
  const resumeCommand = initialValues?.resumeCommand ?? null
  const sameSessionAsSource =
    startedSessionId !== null &&
    startedSessionId === initialValues?.sourceSession.id &&
    (node ?? null) === initialValues.sourceSession.node

  useEffect(() => {
    const wasOpen = wasOpenRef.current
    wasOpenRef.current = open
    if (!open || wasOpen) return
    setCmd(initialValues?.cmd ?? '')
    setArgs(initialValues?.args ?? '')
    setTitle(initialValues?.title ?? '')
    setTags(initialValues?.tags ?? '')
    setCwd(initialValues?.cwd ?? '')
    setNotificationsEnabled(initialValues?.notifications_enabled ?? true)
    setRemoveOriginal(false)
    setStartedSessionId(null)
    setCreationUncertain(false)
    setLoading(false)
    submitPendingRef.current = false
    setResumeApplied(false)
    resumeRestoreRef.current = null
    setError(null)
  }, [initialValues, open])

  async function handleSubmit() {
    if (submitPendingRef.current || creationUncertain) return
    if (!startedSessionId && !cmd.trim()) {
      setError('Command is required')
      return
    }
    submitPendingRef.current = true
    setLoading(true)
    setError(null)
    let newSessionId = startedSessionId
    const source = initialValues?.sourceSession
    try {
      if (!newSessionId) {
        const argList = args.trim() ? parseArgString(args.trim()) : []
        const created = await startSession({
          cmd: cmd.trim(),
          args: argList,
          title: title.trim() || undefined,
          tags: parseSessionTagInput(tags),
          cwd: cwd.trim() || undefined,
          disable_notifications: !notificationsEnabled,
          node: node ?? undefined,
        })
        if (typeof created.session_id !== 'string' || !created.session_id.trim()) {
          setCreationUncertain(true)
          throw new Error(
            'Server did not return a new session ID. Original was not removed; close this dialog and verify the sessions list before trying again.'
          )
        }
        newSessionId = created.session_id.trim()
        if (removeOriginal && source) setStartedSessionId(newSessionId)
      }
      if (removeOriginal && source) {
        // Mirror the TUI's safety check: never delete the newly created session.
        if (newSessionId === source.id && (node ?? null) === source.node) {
          throw new Error('new session has the same ID as the original')
        }
        const { removed } = await forceRemoveSession(source.id, source.node ?? undefined)
        if (!removed) throw new Error('original session was not found')
      }
      onClose()
      if (removeOriginal && source) onRemovedOriginal?.()
      resetForm()
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Operation failed'
      setError(
        newSessionId && removeOriginal && source
          ? `New session ${newSessionId} started, but original ${source.id} was not removed: ${message}. ${newSessionId === source.id && (node ?? null) === source.node ? 'Turn off removal or close this dialog.' : 'Retry removal or close this dialog.'}`
          : message
      )
    } finally {
      submitPendingRef.current = false
      setLoading(false)
    }
  }

  function handleToggleResumeCommand() {
    if (!resumeCommand) return
    if (resumeApplied) {
      const previous = resumeRestoreRef.current
      if (previous) {
        setCmd(previous.cmd)
        setArgs(previous.args)
      }
      resumeRestoreRef.current = null
      setResumeApplied(false)
      return
    }
    resumeRestoreRef.current = { cmd, args }
    const parsed = splitResumeCommand(resumeCommand)
    setCmd(parsed.cmd)
    setArgs(parsed.args)
    setResumeApplied(true)
  }

  function resetForm() {
    setCmd('')
    setArgs('')
    setTitle('')
    setTags('')
    setCwd('')
    setNotificationsEnabled(true)
    setRemoveOriginal(false)
    setStartedSessionId(null)
    setCreationUncertain(false)
    setResumeApplied(false)
    resumeRestoreRef.current = null
    setError(null)
  }

  function handleClose() {
    if (loading) return
    resetForm()
    onClose()
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(nextOpen) => {
        if (!nextOpen && !loading) handleClose()
      }}
    >
      <DialogContent className="max-w-md" showCloseButton={!loading}>
        <DialogHeader>
          <DialogTitle>New Session</DialogTitle>
        </DialogHeader>
        <Form.Root
          onSubmit={(event) => {
            event.preventDefault()
            void handleSubmit()
          }}
          className="mt-1 flex flex-col gap-3"
        >
          <FormField
            name="command"
            label="Command"
            required
            error={error === 'Command is required' ? error : undefined}
          >
            <ClearableInput
              value={cmd}
              onChange={(event) => setCmd(event.target.value)}
              placeholder="claude, bash, python…"
              clearLabel="Clear command"
              required
              autoFocus
              disabled={loading || startedSessionId !== null || creationUncertain}
            />
          </FormField>
          <FormField name="arguments" label="Arguments">
            <ClearableInput
              value={args}
              onChange={(event) => setArgs(event.target.value)}
              placeholder="--model sonnet-3.7 (space-separated)"
              clearLabel="Clear arguments"
              disabled={loading || startedSessionId !== null || creationUncertain}
            />
          </FormField>
          {resumeCommand ? (
            <button
              type="button"
              className="w-full rounded-md p-1 outline-dashed outline-1 outline-neutral-500 opacity-60 hover:opacity-80 focus:opacity-80 -mt-1 whitespace-normal max-h-12 align-top overflow-y-auto"
              onClick={handleToggleResumeCommand}
              disabled={loading || startedSessionId !== null || creationUncertain}
            >
              {resumeApplied ? 'Revert: ' : 'Resume: '}
              {resumeCommand}
            </button>
          ) : null}
          <FormField name="title" label="Title">
            <ClearableInput
              value={title}
              onChange={(event) => setTitle(event.target.value)}
              placeholder="Optional display name"
              clearLabel="Clear title"
              disabled={loading || startedSessionId !== null || creationUncertain}
            />
          </FormField>
          <FormField name="tags" label="Tags">
            <ClearableInput
              value={tags}
              onChange={(event) => setTags(event.target.value)}
              placeholder="prod, release (Separate tags with commas)"
              clearLabel="Clear tags"
              disabled={loading || startedSessionId !== null || creationUncertain}
            />
          </FormField>
          <FormField name="cwd" label="Working Directory">
            <ClearableInput
              value={cwd}
              onChange={(event) => setCwd(event.target.value)}
              placeholder="/path/to/project"
              clearLabel="Clear working directory"
              disabled={loading || startedSessionId !== null || creationUncertain}
            />
          </FormField>
          <NotificationToggle
            checked={notificationsEnabled}
            onCheckedChange={setNotificationsEnabled}
            disabled={loading || startedSessionId !== null || creationUncertain}
          />
          {initialValues?.sourceSession && (
            <div
              className={cn(
                'flex items-center justify-between gap-3 rounded-md border px-3 py-2.5 transition-colors',
                removeOriginal
                  ? 'border-[hsl(var(--destructive))]/50 bg-[hsl(var(--destructive))]/10'
                  : 'border-[hsl(var(--border))] bg-[hsl(var(--muted))]/40'
              )}
            >
              <div className="flex min-w-0 flex-col gap-0.5">
                <label
                  htmlFor="remove-original-session"
                  className={cn(
                    'cursor-pointer text-xs font-medium',
                    removeOriginal
                      ? 'text-[hsl(var(--destructive))]'
                      : 'text-[hsl(var(--foreground))]'
                  )}
                >
                  Remove original session
                </label>
                <span className="text-[11px] text-[hsl(var(--muted-foreground))]">
                  After starting, force-remove the original (kill it and delete its files).
                </span>
              </div>
              <Switch
                id="remove-original-session"
                checked={removeOriginal}
                disabled={loading}
                onCheckedChange={setRemoveOriginal}
                aria-label="Remove original session"
                tone="destructive"
              />
            </div>
          )}
          {error && error !== 'Command is required' ? <FormError>{error}</FormError> : null}
          <FormActions>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              onClick={handleClose}
              disabled={loading}
            >
              {startedSessionId || creationUncertain ? 'Close' : 'Cancel'}
            </Button>
            <Button
              type="submit"
              size="sm"
              variant={removeOriginal ? 'destructive' : 'default'}
              disabled={loading || creationUncertain || (sameSessionAsSource && removeOriginal)}
            >
              {loading
                ? startedSessionId
                  ? 'Removing…'
                  : 'Starting…'
                : startedSessionId
                  ? removeOriginal
                    ? 'Retry removal'
                    : 'Keep original'
                  : removeOriginal
                    ? 'Start & remove original'
                    : 'Start Session'}
            </Button>
          </FormActions>
        </Form.Root>
      </DialogContent>
    </Dialog>
  )
}
