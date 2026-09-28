import SparklineSvg from '@/components/SparklineSvg'
import { useSessionActivitySeries } from '@/lib/sessionActivity'

export default function SessionActivitySparkline({
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
  const series = useSessionActivitySeries(sessionId, node)

  return (
    <SparklineSvg
      series={series}
      fullWidth={fullWidth}
      height={height}
      className={className}
      enableAnimation={isRunning}
    />
  )
}
