import { useEffect, useMemo, useRef, useState } from 'react'

import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'

const SPARKLINE_NUM_BUCKETS = 40
const SPARKLINE_BUCKET_MS = 2_000
const SPARKLINE_BUCKET_SECONDS = SPARKLINE_BUCKET_MS / 1_000
const RECENT_RATE_BUCKETS = 5

interface Props {
  series: number[]
  width?: number
  height?: number
  fullWidth?: boolean
  className?: string
  enableAnimation: boolean
}

type SparklinePoint = {
  x: number
  y: number
}

type SparklinePalette = {
  stroke: string
  strokeSoft: string
  fill: string
  dot: string
  baseline: string
}

const RUNNING_PALETTE: SparklinePalette = {
  stroke: '#16A34A',
  strokeSoft: '#22C55E55',
  fill: '#22C55E14',
  dot: '#4ADE80',
  baseline: '#22C55E33',
}

const IDLE_PALETTE: SparklinePalette = {
  stroke: '#94A3B8',
  strokeSoft: '#CBD5E144',
  fill: '#CBD5E112',
  dot: '#CBD5E1',
  baseline: '#CBD5E133',
}

export default function SparklineSvg({
  series,
  width = 80,
  height = 22,
  fullWidth = false,
  className,
  enableAnimation,
}: Props) {
  const hostRef = useRef<HTMLSpanElement | null>(null)
  const [measuredWidth, setMeasuredWidth] = useState<number>(width)

  useEffect(() => {
    if (!fullWidth) return
    const el = hostRef.current
    if (!el) return
    const update = () => {
      const next = Math.max(1, Math.round(el.getBoundingClientRect().width))
      setMeasuredWidth((prev) => (prev === next ? prev : next))
    }
    update()
    if (typeof ResizeObserver === 'undefined') return
    const observer = new ResizeObserver(() => update())
    observer.observe(el)
    return () => observer.disconnect()
  }, [fullWidth])

  const renderWidth = fullWidth ? measuredWidth : width
  const classes = fullWidth
    ? `block w-full align-middle ${className ?? ''}`.trim()
    : `inline-block align-middle ${className ?? ''}`.trim()

  const recentRate = useMemo(() => calculateRecentBytesPerSecond(series), [series])
  const peakRate = useMemo(() => calculatePeakBytesPerSecond(series), [series])
  const averageRate = useMemo(() => calculateAverageBytesPerSecond(series), [series])
  const tooltipLabel = useMemo(
    () =>
      `${enableAnimation ? 'Running' : 'Stopped'} activity\nRecent: ${formatBytesPerSecond(recentRate)}\nPeak: ${formatBytesPerSecond(peakRate)}\nAverage: ${formatBytesPerSecond(averageRate)}`,
    [averageRate, enableAnimation, peakRate, recentRate]
  )
  const model = useMemo(
    () => buildSparklineModel(series, renderWidth, height, enableAnimation),
    [enableAnimation, height, renderWidth, series]
  )

  return (
    <Tooltip delayDuration={150}>
      <TooltipTrigger asChild>
        <span ref={hostRef} className={classes} aria-label={tooltipLabel}>
          <svg
            width={renderWidth}
            height={height}
            viewBox={`0 0 ${renderWidth} ${height}`}
            xmlns="http://www.w3.org/2000/svg"
            className="overflow-visible"
            role="img"
            aria-hidden="true"
          >
            <title>{tooltipLabel}</title>
            <line
              x1="0"
              y1={model.baselineY}
              x2={renderWidth}
              y2={model.baselineY}
              stroke={model.palette.baseline}
              strokeWidth="1"
            />
            <polygon points={model.areaPoints} fill={model.palette.fill} />
            <polyline
              points={model.linePoints}
              fill="none"
              stroke={model.palette.strokeSoft}
              strokeWidth="4"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
            <polyline
              points={model.linePoints}
              fill="none"
              stroke={model.palette.stroke}
              strokeWidth="1.8"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
            <circle cx={model.lastPoint.x} cy={model.lastPoint.y} r="1.7" fill={model.palette.dot}>
              {enableAnimation ? (
                <>
                  <animate
                    attributeName="r"
                    values="1.4;2.1;1.4"
                    dur="1.5s"
                    repeatCount="indefinite"
                  />
                  <animate
                    attributeName="opacity"
                    values="0.7;1;0.7"
                    dur="1.5s"
                    repeatCount="indefinite"
                  />
                </>
              ) : null}
            </circle>
          </svg>
        </span>
      </TooltipTrigger>
      <TooltipContent side="top" align="center">
        <div className="text-xs leading-tight">
          <div className="font-medium">{enableAnimation ? 'Running' : 'Stopped'} activity</div>
          <div className="text-[hsl(var(--muted-foreground))]">
            Recent {formatBytesPerSecond(recentRate)}
          </div>
          <div className="text-[hsl(var(--muted-foreground))]">
            Peak {formatBytesPerSecond(peakRate)}
          </div>
          <div className="text-[hsl(var(--muted-foreground))]">
            Average {formatBytesPerSecond(averageRate)}
          </div>
        </div>
      </TooltipContent>
    </Tooltip>
  )
}

// ---------------------------------------------------------------------------
// Sparkline data
// ---------------------------------------------------------------------------

/** Rolling activity history using time-bucketed counts (bucketMs-wide slots). */
export class SparklineStore {
  private readonly numBuckets = SPARKLINE_NUM_BUCKETS
  private readonly bucketMs = SPARKLINE_BUCKET_MS

  private data = new Map<
    string,
    { counts: number[]; lastBucket: number; lastTotalBytes: number | null }
  >()

  private nowBucket(): number {
    return Math.floor(Date.now() / this.bucketMs)
  }

  private getOrCreate(id: string) {
    let entry = this.data.get(id)
    if (!entry) {
      entry = {
        counts: new Array(this.numBuckets).fill(0),
        lastBucket: this.nowBucket(),
        lastTotalBytes: null,
      }
      this.data.set(id, entry)
    }
    return entry
  }

  /** Advance the ring buffer to the current bucket, zero-filling gaps. */
  private advance(entry: { counts: number[]; lastBucket: number; lastTotalBytes: number | null }) {
    const now = this.nowBucket()
    const delta = now - entry.lastBucket
    if (delta <= 0) return
    const gap = Math.min(delta, this.numBuckets)
    for (let i = 0; i < gap; i++) {
      entry.counts.shift()
      entry.counts.push(0)
    }
    entry.lastBucket = now
  }

  ensure(id: string): void {
    this.getOrCreate(id)
  }

  /** Increment the current time bucket for this session. */
  touch(id: string, value = 1): void {
    const entry = this.getOrCreate(id)
    this.advance(entry)
    entry.counts[entry.counts.length - 1] += value
  }

  /** Record absolute byte totals and add the positive delta into the current bucket. */
  recordTotal(id: string, totalBytes: number, previousTotalBytes?: number): void {
    const entry = this.getOrCreate(id)
    this.advance(entry)
    const baseline = previousTotalBytes ?? entry.lastTotalBytes ?? totalBytes
    const delta = Math.max(totalBytes - baseline, 0)
    entry.lastTotalBytes = totalBytes
    if (delta > 0) entry.counts[entry.counts.length - 1] += delta
  }

  /** Returns the current bucket series (advances to now first). */
  getSeries(id: string): number[] {
    const entry = this.data.get(id)
    if (!entry) return new Array(this.numBuckets).fill(0)
    this.advance(entry)
    return [...entry.counts]
  }

  remove(id: string) {
    this.data.delete(id)
  }
}

function buildSparklineModel(
  series: number[],
  width: number,
  height: number,
  isRunning: boolean
): {
  areaPoints: string
  linePoints: string
  lastPoint: SparklinePoint
  baselineY: number
  palette: SparklinePalette
} {
  const baselineY = Math.max(2, height - 4)
  const palette = isRunning ? RUNNING_PALETTE : IDLE_PALETTE
  const points = buildSparklinePoints(series, width, height)
  const linePoints = points.map((point) => `${point.x.toFixed(2)},${point.y.toFixed(2)}`).join(' ')
  const areaPoints = [
    `0,${baselineY.toFixed(2)}`,
    ...points.map((point) => `${point.x.toFixed(2)},${point.y.toFixed(2)}`),
    `${width.toFixed(2)},${baselineY.toFixed(2)}`,
  ].join(' ')

  return {
    areaPoints,
    linePoints,
    lastPoint: points[points.length - 1] ?? { x: width, y: baselineY },
    baselineY,
    palette,
  }
}

function buildSparklinePoints(series: number[], width: number, height: number): SparklinePoint[] {
  if (series.length < 2) {
    const baselineY = Math.max(2, height - 4)
    return [
      { x: 0, y: baselineY },
      { x: width, y: baselineY },
    ]
  }

  const maxValue = Math.max(...series, 0)
  const topPadding = 3
  const bottomPadding = 4
  const range = Math.max(height - topPadding - bottomPadding, 1)
  const step = width / (series.length - 1)

  return series.map((value, index) => {
    const x = index * step
    const normalized = maxValue <= 0 ? 0 : Math.log10(value + 1) / Math.log10(maxValue + 1)
    const emphasis = normalized <= 0 ? 0 : Math.pow(normalized, 0.85)
    const y = height - bottomPadding - emphasis * range
    return { x, y }
  })
}

function calculateRecentBytesPerSecond(series: number[]): number {
  const recent = series.slice(-RECENT_RATE_BUCKETS)
  if (recent.length === 0) return 0
  const total = recent.reduce((sum, value) => sum + value, 0)
  return total / (recent.length * SPARKLINE_BUCKET_SECONDS)
}

function calculatePeakBytesPerSecond(series: number[]): number {
  return Math.max(...series, 0) / SPARKLINE_BUCKET_SECONDS
}

function calculateAverageBytesPerSecond(series: number[]): number {
  if (series.length === 0) return 0
  const total = series.reduce((sum, value) => sum + value, 0)
  return total / (series.length * SPARKLINE_BUCKET_SECONDS)
}

function formatBytesPerSecond(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return '0 B/s'
  const units = ['B/s', 'KiB/s', 'MiB/s', 'GiB/s', 'TiB/s']
  let scaled = value
  let unitIndex = 0
  while (scaled >= 1024 && unitIndex < units.length - 1) {
    scaled /= 1024
    unitIndex += 1
  }
  const digits = scaled >= 100 || unitIndex === 0 ? 0 : scaled >= 10 ? 1 : 2
  return `${scaled.toFixed(digits)} ${units[unitIndex]}`
}
