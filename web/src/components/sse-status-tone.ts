import type { SseConnectionState } from '@/api/client'

export type SseStatusTone = {
  label: string
  /** Dot background classes. */
  dot: string
  /** Only genuinely degraded states may pulse for attention. */
  pulse: boolean
}

const TONES: Record<SseConnectionState, SseStatusTone> = {
  live: { label: 'Live', dot: 'bg-green-500', pulse: false },
  // The first handshake is benign: it must not read as an error.
  connecting: { label: 'Connecting…', dot: 'bg-[hsl(var(--muted-foreground))]', pulse: false },
  reconnecting: { label: 'Reconnecting…', dot: 'bg-red-600', pulse: true },
  offline: { label: 'Offline', dot: 'bg-amber-500', pulse: true },
}

export function sseStatusTone(status: SseConnectionState): SseStatusTone {
  return TONES[status] ?? TONES.connecting
}
