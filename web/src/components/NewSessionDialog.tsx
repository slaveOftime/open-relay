import { useEffect, useRef, useState } from 'react'
import * as Form from '@radix-ui/react-form'
import { startSession } from '@/api/client'
import { parseSessionTagInput } from '@/lib/sessionMetadata'
import { parseArgString } from '@/utils/format'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { FormActions, FormError, FormField } from '@/components/ui/form-field'
import type { NewSessionInitialValues } from './new-session-dialog-values'

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
          className="mt-1 flex flex-col gap-4"
        >
          <FormField
            name="command"
            label="Command"
            required
            error={error === 'Command is required' ? error : undefined}
          >
            <Input
              value={cmd}
              onChange={(event) => setCmd(event.target.value)}
              placeholder="claude, bash, python…"
              required
              autoFocus
            />
          </FormField>
          <FormField name="arguments" label="Arguments">
            <Input
              value={args}
              onChange={(event) => setArgs(event.target.value)}
              placeholder="--model sonnet-3.7 (space-separated)"
            />
          </FormField>
          <FormField name="title" label="Title">
            <Input
              value={title}
              onChange={(event) => setTitle(event.target.value)}
              placeholder="Optional display name"
            />
          </FormField>
          <FormField name="tags" label="Tags" description="Separate tags with commas.">
            <Input
              value={tags}
              onChange={(event) => setTags(event.target.value)}
              placeholder="prod, release"
            />
          </FormField>
          <FormField name="cwd" label="Working Directory">
            <Input
              value={cwd}
              onChange={(event) => setCwd(event.target.value)}
              placeholder="/path/to/project"
            />
          </FormField>
          {error && error !== 'Command is required' ? <FormError>{error}</FormError> : null}
          <FormActions>
            <Button type="button" variant="ghost" size="sm" onClick={handleClose}>
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={loading}>
              {loading ? 'Starting…' : 'Start Session'}
            </Button>
          </FormActions>
        </Form.Root>
      </DialogContent>
    </Dialog>
  )
}
