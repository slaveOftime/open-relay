import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from 'react'

import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'
import {
  advanceTimedBuckets,
  animatedHeadY,
  buildAreaPath,
  buildSmoothLinePath,
  seedHeldBuckets,
  sparklinePointsFromHeights,
} from './sparklineGeometry'
import { buildSparklineModel } from './sparklineModel'
import { SPARKLINE_BUCKET_MS, type SparklineActivitySnapshot } from '@/lib/sparklineStore'
import {
  calculateAverageBytesPerSecond,
  calculatePeakBytesPerSecond,
  calculateRecentBytesPerSecond,
  calculateSparklineLastY,
  carryOpenBucket,
  formatBytesPerSecond,
} from './sparklineMetrics'
interface Props {
  width?: number
  readActivity: () => SparklineActivitySnapshot
  subscribeActivity: (listener: () => void) => () => void
  height?: number
  fullWidth?: boolean
  className?: string
  enableAnimation: boolean
}

/** Head transition and the frame budget the animation loop spends per tick. */
const HEAD_TRANSITION_MS = 220
const FRAME_MS = 1000 / 60

function activityTooltipLabel(series: number[], isRunning: boolean): string {
  return `${isRunning ? 'Running' : 'Stopped'} activity\nRecent: ${formatBytesPerSecond(calculateRecentBytesPerSecond(series))}\nPeak: ${formatBytesPerSecond(calculatePeakBytesPerSecond(series))}\nAverage: ${formatBytesPerSecond(calculateAverageBytesPerSecond(series))}`
}

type SparklineVisualState = {
  heights: number[]
  bucketIndex: number
  lastOutputAt: number | null
  headFrom: number
  headTarget: number
  headChangedAt: number | null
  baseline: number
  width: number
  height: number
}
export default function SparklineSvg({
  readActivity,
  subscribeActivity,
  width = 80,
  height = 22,
  fullWidth = false,
  className,
  enableAnimation,
}: Props) {
  const series = readActivity().series
  const [tooltipSeries, setTooltipSeries] = useState(series)
  const [tooltipOpen, setTooltipOpen] = useState(false)
  const tooltipOpenRef = useRef(false)
  const metricSeries = tooltipOpen ? tooltipSeries : series
  const hostRef = useRef<HTMLSpanElement | null>(null)
  const [measuredWidth, setMeasuredWidth] = useState<number>(width)
  const gradientSeed = useId().replace(/[^a-zA-Z0-9_-]/g, '')
  const titleRef = useRef<SVGTitleElement>(null)
  const labelRef = useRef<string | null>(null)

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

  const recentRate = useMemo(() => calculateRecentBytesPerSecond(metricSeries), [metricSeries])
  const peakRate = useMemo(() => calculatePeakBytesPerSecond(metricSeries), [metricSeries])
  const averageRate = useMemo(() => calculateAverageBytesPerSecond(metricSeries), [metricSeries])
  const tooltipLabel = useMemo(
    () => activityTooltipLabel(metricSeries, enableAnimation),
    [enableAnimation, metricSeries]
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
  const visualStateRef = useRef<SparklineVisualState | null>(null)
  const scrollGroupRef = useRef<SVGGElement>(null)
  const headPathRef = useRef<SVGPathElement>(null)
  const headAreaRef = useRef<SVGPathElement>(null)
  const frameRef = useRef<number | null>(null)
  const frameRendererRef = useRef<((now: number) => boolean) | null>(null)
  const redrawBodyRef = useRef<(() => void) | null>(null)
  const requestFrameRef = useRef<() => void>(() => {})
  const canAnimateRef = useRef(false)
  const frameBudgetRef = useRef(0)
  const lastFrameAtRef = useRef(0)
  const reducedMotion =
    typeof window !== 'undefined' && window.matchMedia?.('(prefers-reduced-motion: reduce)').matches

  useLayoutEffect(() => {
    // The heavy body path is rebuilt only when a bucket shifts (~2 Hz).
    const drawBody = (heights: number[], baseline: number, chartWidth: number) => {
      const points = sparklinePointsFromHeights(heights, chartWidth)
      const line = buildSmoothLinePath(points)
      areaPathRef.current?.setAttribute('d', buildAreaPath(points, baseline, line))
      for (const path of [glowPathRef, highlightPathRef, linePathRef, shinePathRef]) {
        path.current?.setAttribute('d', line)
      }
    }
    // Per-frame work: one group transform plus a tiny head segment at the
    // right edge. No 40-point path strings are rebuilt at 60 FPS anymore.
    const drawHead = (state: SparklineVisualState, headY: number, dx: number) => {
      const step = state.width / Math.max(1, state.heights.length - 1)
      scrollGroupRef.current?.setAttribute(
        'transform',
        `translate(${dx === 0 ? 0 : -Math.min(dx, step).toFixed(2)} 0)`
      )
      const x0 = state.width - dx
      const y0 = state.heights[state.heights.length - 1] ?? state.baseline
      const segment = `M ${x0.toFixed(2)} ${y0.toFixed(2)} L ${state.width} ${headY.toFixed(2)}`
      headPathRef.current?.setAttribute('d', segment)
      headAreaRef.current?.setAttribute(
        'd',
        `${segment} L ${state.width} ${state.baseline.toFixed(2)} L ${x0.toFixed(2)} ${state.baseline.toFixed(2)} Z`
      )
      if (lastDotRef.current) {
        lastDotRef.current.setAttribute('cx', String(state.width))
        lastDotRef.current.setAttribute('cy', headY.toFixed(2))
      }
    }

    const visible = Boolean(hostRef.current?.getClientRects().length)
    const shouldAnimate = enableAnimation && !reducedMotion && visible
    canAnimateRef.current = shouldAnimate && !document.hidden
    const previous = visualStateRef.current
    const geometryChanged =
      previous !== null && (previous.width !== renderWidth || previous.height !== height)
    const reset = previous === null || geometryChanged || !enableAnimation || reducedMotion
    const now = Date.now()
    const snapshot = readActivity()

    if (reset) {
      if (frameRef.current !== null) cancelAnimationFrame(frameRef.current)
      frameRef.current = null
      const heights = model.points.map((point) => point.y)
      let lastIndex = snapshot.series.length - 1
      while (lastIndex >= 0 && snapshot.series[lastIndex] <= 0) lastIndex--
      const observedY = lastIndex < 0 ? model.baselineY : model.points[lastIndex].y
      visualStateRef.current = {
        heights: seedHeldBuckets(
          heights,
          lastIndex,
          snapshot.bucketIndex,
          SPARKLINE_BUCKET_MS,
          snapshot.lastOutputAt,
          model.baselineY
        ),
        bucketIndex: snapshot.bucketIndex,
        lastOutputAt: snapshot.lastOutputAt,
        headFrom: observedY,
        headTarget: model.lastPoint.y,
        headChangedAt: null,
        baseline: model.baselineY,
        width: renderWidth,
        height,
      }
    }

    const renderFrame = (frameNow: number) => {
      const state = visualStateRef.current
      if (!state) return false
      const currentBucket = Math.floor(frameNow / SPARKLINE_BUCKET_MS)
      if (currentBucket !== state.bucketIndex) {
        state.heights = advanceTimedBuckets(
          state.heights,
          state.bucketIndex,
          currentBucket,
          SPARKLINE_BUCKET_MS,
          state.lastOutputAt,
          state.headTarget,
          state.baseline
        )
        state.bucketIndex = currentBucket
        drawBody(state.heights, state.baseline, state.width)
      }
      const head = animatedHeadY(
        frameNow,
        state.headFrom,
        state.headTarget,
        state.headChangedAt,
        state.lastOutputAt,
        state.baseline,
        HEAD_TRANSITION_MS
      )
      const step = state.width / Math.max(1, state.heights.length - 1)
      const phase =
        (((frameNow % SPARKLINE_BUCKET_MS) + SPARKLINE_BUCKET_MS) % SPARKLINE_BUCKET_MS) /
        SPARKLINE_BUCKET_MS
      drawHead(state, head, phase * step)
      const headIsStarting =
        state.headChangedAt !== null &&
        frameNow - state.headChangedAt < HEAD_TRANSITION_MS &&
        state.headTarget < state.baseline - 0.01
      return headIsStarting || state.heights.some((y) => y < state.baseline - 0.01)
    }
    frameRendererRef.current = renderFrame
    redrawBodyRef.current = () => {
      const state = visualStateRef.current
      if (state) drawBody(state.heights, state.baseline, state.width)
    }

    const scheduleFrame = () => {
      if (!canAnimateRef.current || frameRef.current !== null) return
      lastFrameAtRef.current = performance.now()
      frameBudgetRef.current = FRAME_MS
      const tick = (frameTime: number) => {
        frameRef.current = null
        if (!canAnimateRef.current || document.hidden) return
        frameBudgetRef.current += Math.max(0, frameTime - lastFrameAtRef.current)
        lastFrameAtRef.current = frameTime
        if (frameBudgetRef.current >= FRAME_MS) {
          frameBudgetRef.current %= FRAME_MS
          if (!frameRendererRef.current?.(Date.now())) return
        }
        frameRef.current = requestAnimationFrame(tick)
      }
      frameRef.current = requestAnimationFrame(tick)
    }
    requestFrameRef.current = scheduleFrame

    if (!enableAnimation || reducedMotion) {
      canAnimateRef.current = false
      if (frameRef.current !== null) cancelAnimationFrame(frameRef.current)
      frameRef.current = null
      // Reset above always rebuilds visualStateRef for static modes, so the
      // head helpers can draw the committed snapshot with no scroll offset.
      drawBody(
        model.points.map((point) => point.y),
        model.baselineY,
        renderWidth
      )
      const staticState = visualStateRef.current
      if (staticState) drawHead(staticState, model.lastPoint.y, 0)
      return
    }

    if (!visible) {
      if (frameRef.current !== null) cancelAnimationFrame(frameRef.current)
      frameRef.current = null
      return
    }

    const state = visualStateRef.current
    if (state) drawBody(state.heights, state.baseline, state.width)
    if (frameRef.current === null && renderFrame(now)) scheduleFrame()
  }, [
    enableAnimation,
    height,
    model.baselineY,
    model.lastPoint.y,
    model.points,
    readActivity,
    reducedMotion,
    renderWidth,
    series,
  ])

  // SSE writes update refs/SVG directly, not React state. In particular this
  // subscriber never tears down the 60 FPS clock on each incoming summary.
  useLayoutEffect(() => {
    function syncActivity() {
      const next = readActivity()
      const state = visualStateRef.current
      if (!state) return
      if (next.lastOutputAt !== null && next.lastOutputAt !== state.lastOutputAt) {
        const now = Date.now()
        const before = animatedHeadY(
          now,
          state.headFrom,
          state.headTarget,
          state.headChangedAt,
          state.lastOutputAt,
          state.baseline,
          HEAD_TRANSITION_MS
        )
        state.heights = advanceTimedBuckets(
          state.heights,
          state.bucketIndex,
          Math.floor(now / SPARKLINE_BUCKET_MS),
          SPARKLINE_BUCKET_MS,
          state.lastOutputAt,
          state.headTarget,
          state.baseline
        )
        state.bucketIndex = Math.floor(now / SPARKLINE_BUCKET_MS)
        // The shift may have rewritten committed buckets; repaint the body
        // now instead of waiting for the next animation-frame bucket check.
        redrawBodyRef.current?.()
        state.headFrom = before
        state.headTarget = calculateSparklineLastY(carryOpenBucket(next.series), state.height)
        state.headChangedAt = now
        state.lastOutputAt = next.lastOutputAt
      }
      const label = activityTooltipLabel(next.series, enableAnimation)
      // Only touch the DOM when the text actually changed; every SSE summary
      // would otherwise invalidate style/layout for the row at ~2 Hz.
      if (label !== labelRef.current) {
        labelRef.current = label
        hostRef.current?.setAttribute('aria-label', label)
        const title = titleRef.current
        if (title) {
          if (title.firstChild) title.firstChild.textContent = label
          else title.appendChild(document.createTextNode(label))
        }
      }
      if (tooltipOpenRef.current) setTooltipSeries(next.series)
      requestFrameRef.current()
    }

    const unsubscribe = subscribeActivity(syncActivity)
    syncActivity() // close the render/subscribe race without forcing a React render
    return unsubscribe
  }, [enableAnimation, readActivity, subscribeActivity])
  const areaGradientId = `${gradientSeed}-area`
  const glowGradientId = `${gradientSeed}-glow`
  const bodyClipId = `${gradientSeed}-clip`

  return (
    <Tooltip
      delayDuration={150}
      onOpenChange={(open) => {
        tooltipOpenRef.current = open
        setTooltipOpen(open)
        if (open) setTooltipSeries(readActivity().series)
      }}
    >
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
              <clipPath id={bodyClipId}>
                <rect x="0" y="-6" width={renderWidth} height={height + 12} />
              </clipPath>
            </defs>
            <line
              x1="0"
              y1={model.baselineY}
              x2={renderWidth}
              y2={model.baselineY}
              stroke={model.palette.baseline}
              strokeWidth="1"
            />
            {/* The scrolling body: one transform per frame, path geometry is
                rebuilt only when a bucket shifts. */}
            <g ref={scrollGroupRef} clipPath={`url(#${bodyClipId})`} transform="translate(0 0)">
              <path ref={areaPathRef} d="" fill={`url(#${areaGradientId})`} />
              <path
                ref={glowPathRef}
                d=""
                fill="none"
                stroke={`url(#${glowGradientId})`}
                strokeWidth="5.5"
                strokeLinecap="round"
                strokeLinejoin="round"
                opacity="0.12"
              />
              <path
                ref={highlightPathRef}
                d=""
                fill="none"
                stroke={`url(#${glowGradientId})`}
                strokeWidth="3.4"
                strokeLinecap="round"
                strokeLinejoin="round"
                opacity="0.24"
              />
              <path
                ref={linePathRef}
                d=""
                fill="none"
                stroke={model.palette.stroke}
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              />
              <path
                ref={shinePathRef}
                d=""
                fill="none"
                stroke={model.palette.strokeHighlight}
                strokeWidth="0.75"
                strokeLinecap="round"
                strokeLinejoin="round"
                opacity="0.62"
              />
            </g>
            {/* Tiny head segment pinned to the right edge; rebuilt every frame
                but only ever two points long. */}
            <path ref={headAreaRef} d="" fill={`url(#${areaGradientId})`} />
            <path
              ref={headPathRef}
              d=""
              fill="none"
              stroke={model.palette.stroke}
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
            <circle ref={lastDotRef} cx={0} cy={0} r="1.7" fill={model.palette.dot} opacity="0.72">
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
