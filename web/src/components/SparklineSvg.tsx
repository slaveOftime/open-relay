import { useEffect, useId, useMemo, useRef, useState } from 'react'

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
  strokeHighlight: string
  glow: string
  fillTop: string
  fillBottom: string
  dot: string
  baseline: string
}

const RUNNING_PALETTE: SparklinePalette = {
  stroke: '#34C85B',
  strokeHighlight: '#8EF5AB',
  glow: '#2BC851A6',
  fillTop: '#2EEA6B30',
  fillBottom: '#0A130B00',
  dot: '#B5F8C8',
  baseline: '#23442A',
}

const IDLE_PALETTE: SparklinePalette = {
  stroke: '#7D8B97',
  strokeHighlight: '#C5D0D8',
  glow: '#32404B66',
  fillTop: '#7D8B9724',
  fillBottom: '#11181D00',
  dot: '#D6DEE4',
  baseline: '#24303A',
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
  const gradientSeed = useId().replace(/[^a-zA-Z0-9_-]/g, '')

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
  const areaGradientId = `${gradientSeed}-area`
  const glowGradientId = `${gradientSeed}-glow`

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
            <defs>
              <linearGradient id={areaGradientId} x1="0" y1="0" x2="0" y2="1">
                <stop offset="0%" stopColor={model.palette.fillTop} />
                <stop offset="100%" stopColor={model.palette.fillBottom} />
              </linearGradient>
              <linearGradient id={glowGradientId} x1="0" y1="0" x2="0" y2="1">
                <stop offset="0%" stopColor={model.palette.strokeHighlight} stopOpacity="0.8" />
                <stop offset="100%" stopColor={model.palette.glow} stopOpacity="0.9" />
              </linearGradient>
            </defs>
            <line
              x1="0"
              y1={model.baselineY}
              x2={renderWidth}
              y2={model.baselineY}
              stroke={model.palette.baseline}
              strokeWidth="1"
            />
            <path d={model.areaPath} fill={`url(#${areaGradientId})`} />
            <path
              d={model.linePath}
              fill="none"
              stroke={`url(#${glowGradientId})`}
              strokeWidth="5.5"
              strokeLinecap="round"
              strokeLinejoin="round"
              opacity="0.12"
            />
            <path
              d={model.linePath}
              fill="none"
              stroke={`url(#${glowGradientId})`}
              strokeWidth="3.4"
              strokeLinecap="round"
              strokeLinejoin="round"
              opacity="0.24"
            />
            <path
              d={model.linePath}
              fill="none"
              stroke={model.palette.stroke}
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
            <path
              d={model.linePath}
              fill="none"
              stroke={model.palette.strokeHighlight}
              strokeWidth="0.75"
              strokeLinecap="round"
              strokeLinejoin="round"
              opacity="0.62"
            />
            <circle cx={model.lastPoint.x} cy={model.lastPoint.y} r="1.7" fill={model.palette.dot} opacity="0.72">
              {enableAnimation ? (
                <>
                  <animate
                    attributeName="r"
                    values="1.3;2;1.3"
                    dur="1.8s"
                    repeatCount="indefinite"
                  />
                  <animate
                    attributeName="opacity"
                    values="0.65;1;0.65"
                    dur="1.8s"
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
  private readonly listeners = new Set<() => void>()
  private readonly emptySeries = new Array(this.numBuckets).fill(0)
  private decayTimer: ReturnType<typeof setInterval> | null = null

  private data = new Map<
    string,
    { counts: number[]; snapshot: number[]; lastBucket: number; lastTotalBytes: number | null }
  >()

  private nowBucket(): number {
    return Math.floor(Date.now() / this.bucketMs)
  }

  private getOrCreate(id: string) {
    let entry = this.data.get(id)
    if (!entry) {
      const counts = new Array(this.numBuckets).fill(0)
      entry = {
        counts,
        snapshot: [...counts],
        lastBucket: this.nowBucket(),
        lastTotalBytes: null,
      }
      this.data.set(id, entry)
    }
    return entry
  }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener)
    this.ensureDecayTimer()
    return () => {
      this.listeners.delete(listener)
      if (this.listeners.size === 0) {
        this.stopDecayTimer()
      }
    }
  }

  private emitChange(): void {
    this.listeners.forEach((listener) => listener())
  }

  private ensureDecayTimer(): void {
    if (this.decayTimer !== null || typeof window === 'undefined') {
      return
    }
    this.decayTimer = window.setInterval(() => {
      let changed = false
      for (const entry of this.data.values()) {
        changed = this.advance(entry) || changed
      }
      if (changed) {
        this.emitChange()
      }
    }, this.bucketMs)
  }

  private stopDecayTimer(): void {
    if (this.decayTimer === null) {
      return
    }
    clearInterval(this.decayTimer)
    this.decayTimer = null
  }

  private refreshSnapshot(entry: {
    counts: number[]
    snapshot: number[]
    lastBucket: number
    lastTotalBytes: number | null
  }): void {
    entry.snapshot = [...entry.counts]
  }

  /** Advance the ring buffer to the current bucket, zero-filling gaps. */
  private advance(entry: {
    counts: number[]
    snapshot: number[]
    lastBucket: number
    lastTotalBytes: number | null
  }): boolean {
    const now = this.nowBucket()
    const delta = now - entry.lastBucket
    if (delta <= 0) return false
    const gap = Math.min(delta, this.numBuckets)
    for (let i = 0; i < gap; i++) {
      entry.counts.shift()
      entry.counts.push(0)
    }
    entry.lastBucket = now
    this.refreshSnapshot(entry)
    return true
  }

  ensure(id: string): void {
    this.getOrCreate(id)
  }

  /** Increment the current time bucket for this session. */
  touch(id: string, value = 1): void {
    const entry = this.getOrCreate(id)
    this.advance(entry)
    entry.counts[entry.counts.length - 1] += value
    this.refreshSnapshot(entry)
    this.emitChange()
  }

  /** Record absolute byte totals and add the positive delta into the current bucket. */
  recordTotal(id: string, totalBytes: number, previousTotalBytes?: number): void {
    const entry = this.getOrCreate(id)
    const advanced = this.advance(entry)
    const baseline = previousTotalBytes ?? entry.lastTotalBytes ?? totalBytes
    const delta = Math.max(totalBytes - baseline, 0)
    entry.lastTotalBytes = totalBytes
    if (delta > 0) {
      entry.counts[entry.counts.length - 1] += delta
      this.refreshSnapshot(entry)
      this.emitChange()
      return
    }
    if (advanced) {
      this.emitChange()
    }
  }

  /** Returns the current bucket series (advances to now first). */
  getSeries(id: string): number[] {
    const entry = this.data.get(id)
    if (!entry) return this.emptySeries
    this.advance(entry)
    return entry.snapshot
  }

  remove(id: string) {
    if (this.data.delete(id)) {
      this.emitChange()
    }
  }
}

function buildSparklineModel(
  series: number[],
  width: number,
  height: number,
  isRunning: boolean
): {
  areaPath: string
  linePath: string
  lastPoint: SparklinePoint
  baselineY: number
  palette: SparklinePalette
} {
  const baselineY = Math.max(2, height - 3)
  const palette = isRunning ? RUNNING_PALETTE : IDLE_PALETTE
  const points = buildSparklinePoints(series, width, height)
  const linePath = buildSmoothLinePath(points)
  const areaPath = buildAreaPath(points, baselineY)

  return {
    areaPath,
    linePath,
    lastPoint: points[points.length - 1] ?? { x: width, y: baselineY },
    baselineY,
    palette,
  }
}

function buildSparklinePoints(series: number[], width: number, height: number): SparklinePoint[] {
  if (series.length < 2) {
    const baselineY = Math.max(2, height - 3)
    return [
      { x: 0, y: baselineY },
      { x: width, y: baselineY },
    ]
  }

  const maxValue = Math.max(...series, 0)
  const topPadding = 2
  const bottomPadding = 3
  const range = Math.max(height - topPadding - bottomPadding, 1)
  const step = width / (series.length - 1)

  return series.map((value, index) => {
    const x = index * step
    const normalized = maxValue <= 0 ? 0 : Math.log10(value + 1) / Math.log10(maxValue + 1)
    const emphasis = normalized <= 0 ? 0 : Math.pow(normalized, 0.86)
    const y = height - bottomPadding - emphasis * range
    return { x, y }
  })
}

function buildSmoothLinePath(points: SparklinePoint[]): string {
  if (points.length === 0) return ''
  const [first, ...rest] = points
  return [
    `M ${first.x.toFixed(2)} ${first.y.toFixed(2)}`,
    ...rest.map((point) => `L ${point.x.toFixed(2)} ${point.y.toFixed(2)}`),
  ].join(' ')
}

function buildAreaPath(points: SparklinePoint[], baselineY: number): string {
  if (points.length === 0) return ''
  const first = points[0]
  const last = points[points.length - 1]
  return [
    `M ${first.x.toFixed(2)} ${baselineY.toFixed(2)}`,
    `L ${first.x.toFixed(2)} ${first.y.toFixed(2)}`,
    buildSmoothLinePath(points).slice(1),
    `L ${last.x.toFixed(2)} ${baselineY.toFixed(2)}`,
    'Z',
  ].join(' ')
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
