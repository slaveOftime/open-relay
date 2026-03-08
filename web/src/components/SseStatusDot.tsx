export default function SseStatusDot({ status }: { status: 'live' | 'reconnecting' | 'offline' }) {
  const isLive = status === 'live'
  const isOffline = status === 'offline'

  return (
    <div
      className={`flex items-center gap-2 text-xs text-[hsl(var(--muted-foreground))] bg-[hsl(var(--card))]/90 px-3 py-1.5 rounded-full backdrop-blur ${isLive ? '' : 'animate-pulse'}`}
    >
      <span
        className={
          isLive
            ? 'inline-block w-2 h-2 rounded-full bg-green-500'
            : isOffline
              ? 'inline-block w-2 h-2 rounded-full bg-amber-500'
              : 'inline-block w-2 h-2 rounded-full bg-red-600'
        }
      />
      <span>{isLive ? 'Live' : isOffline ? 'Offline' : 'Reconnecting…'}</span>
    </div>
  )
}
