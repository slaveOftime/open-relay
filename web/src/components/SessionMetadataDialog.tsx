import { useState } from 'react'
import * as Form from '@radix-ui/react-form'
import { updateSessionMetadata } from '@/api/client'
import type { SessionSummary } from '@/api/types'
import { buildSessionMetadataUpdateSpec, formatSessionTagInput } from '@/lib/sessionMetadata'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog'
import { FormActions, FormError, FormField } from '@/components/ui/form-field'

type SessionMetadataDialogProps = {
  open: boolean
  session: SessionSummary | null
  node?: string
  onClose: () => void
  onSaved: (session: SessionSummary) => void
}

export default function SessionMetadataDialog({
  open,
  session,
  node,
  onClose,
  onSaved,
}: SessionMetadataDialogProps) {
  function handleClose() {
    onClose()
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(nextOpen) => {
        if (!nextOpen) handleClose()
      }}
    >
      {open && session ? (
        <SessionMetadataDialogForm
          key={session.id}
          session={session}
          node={node}
          onClose={handleClose}
          onSaved={onSaved}
        />
      ) : null}
    </Dialog>
  )
}

type SessionMetadataDialogFormProps = {
  session: SessionSummary
  node?: string
  onClose: () => void
  onSaved: (session: SessionSummary) => void
}

function SessionMetadataDialogForm({
  session,
  node,
  onClose,
  onSaved,
}: SessionMetadataDialogFormProps) {
  const [title, setTitle] = useState(() => session?.title ?? '')
  const [tags, setTags] = useState(() => formatSessionTagInput(session?.tags ?? []))
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)

  async function handleSubmit() {
    const spec = buildSessionMetadataUpdateSpec(
      { title: session.title, tags: session.tags },
      { title, tags }
    )
    if (Object.keys(spec).length === 0) {
      onClose()
      return
    }
    setLoading(true)
    setError(null)
    try {
      const updated = await updateSessionMetadata(session.id, spec, node)
      onSaved(updated)
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to update session metadata')
      setLoading(false)
    }
  }

  return (
    <DialogContent className="max-w-md">
      <DialogHeader>
        <DialogTitle>Edit Session</DialogTitle>
      </DialogHeader>
        <Form.Root
          onSubmit={(event) => {
            event.preventDefault()
            void handleSubmit()
          }}
          className="mt-1 flex flex-col gap-4"
        >
          <div className="rounded-md border border-[hsl(var(--border))] bg-[hsl(var(--muted))]/40 px-3 py-2 flex items-center gap-2">
            <div className="text-xs text-[hsl(var(--muted-foreground))]">Session:</div>
            <div className="font-mono text-sm text-[hsl(var(--foreground))] break-all">{session.id}</div>
          </div>
          <FormField
            name="title"
            label="Title"
            description="Leave blank to clear the title."
          >
            <Input
              value={title}
              onChange={(event) => setTitle(event.target.value)}
              placeholder="Optional display name"
              autoFocus
            />
          </FormField>
          <FormField
            name="tags"
            label="Tags"
            description="Separate tags with commas. Leave blank to clear all tags."
          >
            <Input
              value={tags}
              onChange={(event) => setTags(event.target.value)}
              placeholder="prod, release"
            />
          </FormField>
          {error ? <FormError>{error}</FormError> : null}
          <FormActions>
            <Button type="button" variant="ghost" size="sm" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={loading}>
              {loading ? 'Saving…' : 'Save'}
            </Button>
          </FormActions>
        </Form.Root>
      </DialogContent>
    )
}
