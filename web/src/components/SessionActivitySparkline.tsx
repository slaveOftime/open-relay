import { memo, useCallback } from 'react'

import SparklineSvg from '@/components/SparklineSvg'
import {
  getSessionActivitySnapshot,
  sessionActivityKey,
  subscribeSessionActivity,
} from '@/lib/sessionActivity'

function SessionActivitySparkline({
  sessionId,
  node,
  isRunning,
  fullWidth = false,
  height,
  className,
}: {
  sessionId: string
  node?: string | null
  isRunning: boolean
  fullWidth?: boolean
  height?: number
  className?: string
}) {
  const readActivity = useCallback(
    () => getSessionActivitySnapshot(sessionId, node),
    [node, sessionId]
  )
  const subscribeActivity = useCallback(
    (listener: () => void) => subscribeSessionActivity(sessionId, node, listener),
    [node, sessionId]
  )

  return (
    <SparklineSvg
      key={sessionActivityKey(sessionId, node)}
      readActivity={readActivity}
      subscribeActivity={subscribeActivity}
      fullWidth={fullWidth}
      height={height}
      className={className}
      enableAnimation={isRunning}
    />
  )
}

export default memo(SessionActivitySparkline)
