import type { SseConnectionState } from '@/api/client'
import { sseStatusTone } from './sse-status-tone'

export default function SseStatusDot({ status }: { status: SseConnectionState }) {
  const tone = sseStatusTone(status)

  return (
    <div
      className={`flex items-center gap-2 text-xs text-[hsl(var(--muted-foreground))] bg-[hsl(var(--card))]/90 px-3 py-1.5 rounded-full backdrop-blur ${
        tone.pulse ? 'animate-pulse' : ''
      }`}
    >
      <span className={`inline-block w-2 h-2 rounded-full ${tone.dot}`} />
      <span>{tone.label}</span>
    </div>
  )
}
