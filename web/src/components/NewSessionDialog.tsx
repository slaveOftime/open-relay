import { useEffect, useRef, useState } from 'react'
import * as Form from '@radix-ui/react-form'
import { startSession } from '@/api/client'
import type { SessionSummary } from '@/api/types'
import { formatSessionTagInput, parseSessionTagInput } from '@/lib/sessionMetadata'
import { parseArgString } from '@/utils/format'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog'

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

export default function NewSessionDialog({
  open,
  onClose,
  initialValues,
  node,
}: {
  open: boolean
  onClose: () => void
  initialValues?: NewSessionInitialValues
  node?: string
}) {
  const [cmd, setCmd] = useState('')
  const [args, setArgs] = useState('')
  const [title, setTitle] = useState('')
  const [tags, setTags] = useState('')
  const [cwd, setCwd] = useState('')
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const wasOpenRef = useRef(false)

  useEffect(() => {
    const wasOpen = wasOpenRef.current
    wasOpenRef.current = open
    if (!open || wasOpen) return
    setCmd(initialValues?.cmd ?? '')
    setArgs(initialValues?.args ?? '')
    setTitle(initialValues?.title ?? '')
    setTags(initialValues?.tags ?? '')
    setCwd(initialValues?.cwd ?? '')
    setLoading(false)
    setError(null)
  }, [initialValues, open])

  async function handleSubmit() {
    if (!cmd.trim()) {
      setError('Command is required')
      return
    }
    setLoading(true)
    setError(null)
    try {
      const argList = args.trim() ? parseArgString(args.trim()) : []
      await startSession({
        cmd: cmd.trim(),
        args: argList,
        title: title.trim() || undefined,
        tags: parseSessionTagInput(tags),
        cwd: cwd.trim() || undefined,
        node: node ?? undefined,
      })
      onClose()
      resetForm()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to start session')
    } finally {
      setLoading(false)
    }
  }

  function resetForm() {
    setCmd('')
    setArgs('')
    setTitle('')
    setTags('')
    setCwd('')
    setError(null)
  }

  function handleClose() {
    resetForm()
    onClose()
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(nextOpen) => {
        if (!nextOpen) handleClose()
      }}
    >
      <DialogContent className="max-w-md">
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
          <Form.Field name="command" className="flex flex-col gap-1.5">
            <Form.Label className="text-xs text-[hsl(var(--muted-foreground))]">
              Command <span className="text-red-500">*</span>
            </Form.Label>
            <Form.Control asChild>
              <Input
                value={cmd}
                onChange={(event) => setCmd(event.target.value)}
                placeholder="claude, bash, python…"
                required
                autoFocus
              />
            </Form.Control>
            <Form.Message match="valueMissing" className="text-xs text-red-500">
              Command is required
            </Form.Message>
          </Form.Field>
          <Form.Field name="arguments" className="flex flex-col gap-1.5">
            <Form.Label className="text-xs text-[hsl(var(--muted-foreground))]">
              Arguments
            </Form.Label>
            <Form.Control asChild>
              <Input
                value={args}
                onChange={(event) => setArgs(event.target.value)}
                placeholder="--model sonnet-3.7 (space-separated)"
              />
            </Form.Control>
          </Form.Field>
          <Form.Field name="title" className="flex flex-col gap-1.5">
            <Form.Label className="text-xs text-[hsl(var(--muted-foreground))]">Title</Form.Label>
            <Form.Control asChild>
              <Input
                value={title}
                onChange={(event) => setTitle(event.target.value)}
                placeholder="Optional display name"
              />
            </Form.Control>
          </Form.Field>
          <Form.Field name="tags" className="flex flex-col gap-1.5">
            <Form.Label className="text-xs text-[hsl(var(--muted-foreground))]">Tags</Form.Label>
            <Form.Control asChild>
              <Input
                value={tags}
                onChange={(event) => setTags(event.target.value)}
                placeholder="prod, release"
              />
            </Form.Control>
            <p className="text-[11px] text-[hsl(var(--muted-foreground))]">
              Separate tags with commas.
            </p>
          </Form.Field>
          <Form.Field name="cwd" className="flex flex-col gap-1.5">
            <Form.Label className="text-xs text-[hsl(var(--muted-foreground))]">
              Working Directory
            </Form.Label>
            <Form.Control asChild>
              <Input
                value={cwd}
                onChange={(event) => setCwd(event.target.value)}
                placeholder="/path/to/project"
              />
            </Form.Control>
          </Form.Field>
          {error && <p className="text-xs text-red-500">{error}</p>}
          <div className="flex justify-end gap-2 pt-1">
            <Button type="button" variant="ghost" size="sm" onClick={handleClose}>
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={loading}>
              {loading ? 'Starting…' : 'Start Session'}
            </Button>
          </div>
        </Form.Root>
      </DialogContent>
    </Dialog>
  )
}
