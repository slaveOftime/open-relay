import { useEffect, useRef, useState } from 'react'
import * as Form from '@radix-ui/react-form'
import { updateSessionMetadata } from '@/api/client'
import type { SessionSummary } from '@/api/types'
import {
  buildSessionMetadataUpdateSpec,
  formatSessionTagInput,
} from '@/lib/sessionMetadata'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Dialog, DialogContent, DialogHeader, DialogTitle } from '@/components/ui/dialog'

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
  const [title, setTitle] = useState('')
  const [tags, setTags] = useState('')
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const wasOpenRef = useRef(false)

  useEffect(() => {
    const wasOpen = wasOpenRef.current
    wasOpenRef.current = open
    if (!open || wasOpen) return
    setTitle(session?.title ?? '')
    setTags(formatSessionTagInput(session?.tags ?? []))
    setLoading(false)
    setError(null)
  }, [open, session])

  function resetForm() {
    setTitle('')
    setTags('')
    setLoading(false)
    setError(null)
  }

  function handleClose() {
    resetForm()
    onClose()
  }

  async function handleSubmit() {
    if (!session) return
    const spec = buildSessionMetadataUpdateSpec(
      { title: session.title, tags: session.tags },
      { title, tags }
    )
    if (Object.keys(spec).length === 0) {
      handleClose()
      return
    }
    setLoading(true)
    setError(null)
    try {
      const updated = await updateSessionMetadata(session.id, spec, node)
      onSaved(updated)
      handleClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to update session metadata')
      setLoading(false)
    }
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
          <DialogTitle>Edit Session</DialogTitle>
        </DialogHeader>
        <Form.Root
          onSubmit={(event) => {
            event.preventDefault()
            void handleSubmit()
          }}
          className="mt-1 flex flex-col gap-3"
        >
          <div className="rounded-md border border-[hsl(var(--border))] bg-[hsl(var(--muted))]/40 px-3 py-2 flex items-center gap-2">
            <div className="text-xs text-[hsl(var(--muted-foreground))]">Session:</div>
            <div className="font-mono text-sm text-[hsl(var(--foreground))] break-all">
              {session?.id}
            </div>
          </div>
          <Form.Field name="title" className="flex flex-col gap-1.5">
            <Form.Label className="text-xs text-[hsl(var(--muted-foreground))]">Title</Form.Label>
            <Form.Control asChild>
              <Input
                value={title}
                onChange={(event) => setTitle(event.target.value)}
                placeholder="Optional display name"
                autoFocus
              />
            </Form.Control>
            <p className="text-[11px] text-[hsl(var(--muted-foreground))]">
              Leave blank to clear the title.
            </p>
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
              Separate tags with commas. Leave blank to clear all tags.
            </p>
          </Form.Field>
          {error && <p className="text-xs text-red-500">{error}</p>}
          <div className="flex justify-end gap-2 pt-1">
            <Button type="button" variant="ghost" size="sm" onClick={handleClose}>
              Cancel
            </Button>
            <Button type="submit" size="sm" disabled={loading || !session}>
              {loading ? 'Saving…' : 'Save'}
            </Button>
          </div>
        </Form.Root>
      </DialogContent>
    </Dialog>
  )
}
