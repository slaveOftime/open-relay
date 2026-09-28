import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from 'react'

import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'
import {
  animationEase,
  animationWindow,
  interpolateSparklinePoints,
  isBucketShift,
  sameSparklinePoints,
  type SparklinePoint,
} from './sparklineGeometry'
import { SPARKLINE_BUCKET_MS } from './sparklineStore'
import {
  calculateAverageBytesPerSecond,
  calculatePeakBytesPerSecond,
  calculateRecentBytesPerSecond,
  carryOpenBucket,
  formatBytesPerSecond,
} from './sparklineMetrics'

interface Props {
  series: number[]
  width?: number
  height?: number
  fullWidth?: boolean
  className?: string
  enableAnimation: boolean
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
  fillTop: '#34C85B',
  fillBottom: '#34c85b86',
  dot: '#34C85B',
  baseline: '#34c85b86',
}

const TRANSITION_MS = 220
const FRAME_MS = 1000 / 60

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
  const displaySeries = useMemo(() => carryOpenBucket(series), [series])
  const model = useMemo(
    () => buildSparklineModel(displaySeries, renderWidth, height, enableAnimation),
    [displaySeries, enableAnimation, height, renderWidth]
  )
  const areaPathRef = useRef<SVGPathElement>(null)
  const glowPathRef = useRef<SVGPathElement>(null)
  const highlightPathRef = useRef<SVGPathElement>(null)
  const linePathRef = useRef<SVGPathElement>(null)
  const shinePathRef = useRef<SVGPathElement>(null)
  const lastDotRef = useRef<SVGCircleElement>(null)
  const currentPointsRef = useRef<SparklinePoint[] | null>(null)
  const previousSeriesRef = useRef(series)
  const scrollEndAtRef = useRef<number | null>(null)
  const lastWidthRef = useRef(renderWidth)
  const frameRef = useRef<number | null>(null)
  const reducedMotion =
    typeof window !== 'undefined' && window.matchMedia?.('(prefers-reduced-motion: reduce)').matches

  useLayoutEffect(() => {
    const cancel = () => {
      if (frameRef.current !== null) cancelAnimationFrame(frameRef.current)
      frameRef.current = null
    }
    cancel()

    const draw = (points: SparklinePoint[]) => {
      const line = buildSmoothLinePath(points)
      areaPathRef.current?.setAttribute('d', buildAreaPath(points, model.baselineY, line))
      for (const path of [glowPathRef, highlightPathRef, linePathRef, shinePathRef]) {
        path.current?.setAttribute('d', line)
      }
      const last = points[points.length - 1]
      if (lastDotRef.current && last) {
        lastDotRef.current.setAttribute('cx', String(last.x))
        lastDotRef.current.setAttribute('cy', String(last.y))
      }
    }

    const from = currentPointsRef.current
    const bucketShifted = isBucketShift(previousSeriesRef.current, series)
    previousSeriesRef.current = series
    const widthChanged = lastWidthRef.current !== renderWidth
    lastWidthRef.current = renderWidth
    if (
      !from ||
      from.length !== model.points.length ||
      widthChanged ||
      !enableAnimation ||
      reducedMotion ||
      document.hidden ||
      !hostRef.current?.getClientRects().length ||
      sameSparklinePoints(from, model.points)
    ) {
      scrollEndAtRef.current = null
      currentPointsRef.current = model.points
      draw(model.points)
      return cancel
    }

    // React just committed the target d; restore the displayed frame before paint.
    draw(from)
    const startedAt = performance.now()
    const { duration, scrollEnd } = animationWindow(
      startedAt,
      bucketShifted,
      scrollEndAtRef.current,
      SPARKLINE_BUCKET_MS,
      TRANSITION_MS
    )
    scrollEndAtRef.current = scrollEnd
    const scrolling = scrollEnd !== null && scrollEnd > startedAt
    let lastTickAt = startedAt
    let frameBudget = 0
    let painted = false
    const step = (now: number) => {
      frameBudget += Math.max(0, now - lastTickAt)
      lastTickAt = now
      const progress = Math.max(0, Math.min((now - startedAt) / duration, 1))
      if (!painted || progress === 1 || frameBudget >= FRAME_MS) {
        const points = interpolateSparklinePoints(
          from,
          model.points,
          animationEase(progress, scrolling)
        )
        draw(points)
        currentPointsRef.current = points
        frameBudget %= FRAME_MS
        painted = true
      }
      frameRef.current = progress < 1 ? requestAnimationFrame(step) : null
    }
    frameRef.current = requestAnimationFrame(step)
    return cancel
  }, [enableAnimation, model.baselineY, model.points, reducedMotion, renderWidth, series])

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
            <path ref={areaPathRef} d={model.areaPath} fill={`url(#${areaGradientId})`} />
            <path
              ref={glowPathRef}
              d={model.linePath}
              fill="none"
              stroke={`url(#${glowGradientId})`}
              strokeWidth="5.5"
              strokeLinecap="round"
              strokeLinejoin="round"
              opacity="0.12"
            />
            <path
              ref={highlightPathRef}
              d={model.linePath}
              fill="none"
              stroke={`url(#${glowGradientId})`}
              strokeWidth="3.4"
              strokeLinecap="round"
              strokeLinejoin="round"
              opacity="0.24"
            />
            <path
              ref={linePathRef}
              d={model.linePath}
              fill="none"
              stroke={model.palette.stroke}
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
            <path
              ref={shinePathRef}
              d={model.linePath}
              fill="none"
              stroke={model.palette.strokeHighlight}
              strokeWidth="0.75"
              strokeLinecap="round"
              strokeLinejoin="round"
              opacity="0.62"
            />
            <circle
              ref={lastDotRef}
              cx={model.lastPoint.x}
              cy={model.lastPoint.y}
              r="1.7"
              fill={model.palette.dot}
              opacity="0.72"
            >
              {enableAnimation && !reducedMotion ? (
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

function buildSparklineModel(
  series: number[],
  width: number,
  height: number,
  isRunning: boolean
): {
  areaPath: string
  linePath: string
  lastPoint: SparklinePoint
  points: SparklinePoint[]
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
    points,
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

function buildAreaPath(
  points: SparklinePoint[],
  baselineY: number,
  linePath = buildSmoothLinePath(points)
): string {
  if (points.length === 0) return ''
  const first = points[0]
  const last = points[points.length - 1]
  return [
    `M ${first.x.toFixed(2)} ${baselineY.toFixed(2)}`,
    `L ${first.x.toFixed(2)} ${first.y.toFixed(2)}`,
    linePath.slice(1),
    `L ${last.x.toFixed(2)} ${baselineY.toFixed(2)}`,
    'Z',
  ].join(' ')
}
