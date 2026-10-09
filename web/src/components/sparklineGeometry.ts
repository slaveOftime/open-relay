export type SparklinePoint = { x: number; y: number }

export function easeOutCubic(progress: number): number {
  return 1 - (1 - progress) ** 3
}
export const SPARKLINE_HOLD_MS = 400
export const SPARKLINE_FADE_MS = 200

export function animatedHeadY(
  now: number,
  fromY: number,
  targetY: number,
  changedAt: number | null,
  lastOutputAt: number | null,
  baselineY: number,
  transitionMs: number
): number {
  const progress =
    changedAt === null ? 1 : Math.min(1, Math.max(0, (now - changedAt) / transitionMs))
  const observed = fromY + (targetY - fromY) * easeOutCubic(progress)
  return heldHeadY(now, lastOutputAt, observed, baselineY)
}
/** The visual head carries the last observed height, then fades to idle. */
export function heldHeadY(
  now: number,
  lastOutputAt: number | null,
  lastY: number,
  baselineY: number
): number {
  if (lastOutputAt === null) return baselineY
  const silence = now - lastOutputAt
  if (silence <= SPARKLINE_HOLD_MS) return lastY
  const fade = Math.min(1, (silence - SPARKLINE_HOLD_MS) / SPARKLINE_FADE_MS)
  if (fade >= 1) return baselineY
  return lastY + (baselineY - lastY) * fade
}

/** Restore the held plateau when a chart mounts midway through a quiet spell. */
export function seedHeldBuckets(
  heights: number[],
  lastObservedIndex: number,
  bucketIndex: number,
  bucketMs: number,
  lastOutputAt: number | null,
  baselineY: number
): number[] {
  if (lastObservedIndex < 0 || lastOutputAt === null) return heights
  const seeded = heights.slice()
  const lastY = seeded[lastObservedIndex]
  for (let index = lastObservedIndex + 1; index < seeded.length; index++) {
    const start = (bucketIndex - (seeded.length - 1 - index)) * bucketMs
    seeded[index] = heldHeadY(start, lastOutputAt, lastY, baselineY)
  }
  return seeded
}
/** Advance historical heights by wall-clock buckets, even if SSE is quiet. */
export function advanceTimedBuckets(
  heights: number[],
  fromBucket: number,
  toBucket: number,
  bucketMs: number,
  lastOutputAt: number | null,
  lastY: number,
  baselineY: number
): number[] {
  const gap = Math.max(0, toBucket - fromBucket)
  if (gap === 0) return heights
  if (gap >= heights.length) return heights.map(() => baselineY)
  const advanced = heights.slice()
  for (let offset = 1; offset <= gap; offset++) {
    advanced.shift()
    advanced.push(heldHeadY((fromBucket + offset) * bucketMs, lastOutputAt, lastY, baselineY))
  }
  return advanced
}

/** Scroll samples at a constant speed; anchor the held/fading head at the right. */
export function timedSparklinePoints(
  heights: number[],
  now: number,
  bucketMs: number,
  width: number,
  headY: number
): SparklinePoint[] {
  if (heights.length < 2)
    return [
      { x: 0, y: headY },
      { x: width, y: headY },
    ]
  const step = width / (heights.length - 1)
  const phase = (((now % bucketMs) + bucketMs) % bucketMs) / bucketMs
  return [
    ...heights.map((y, index) => ({ x: Math.max(0, (index - phase) * step), y })),
    { x: width, y: headY },
  ]
}

export function sparklinePointsFromHeights(heights: number[], width: number): SparklinePoint[] {
  if (heights.length < 2) {
    const y = heights[0] ?? 0
    return [
      { x: 0, y },
      { x: width, y },
    ]
  }
  const step = width / (heights.length - 1)
  return heights.map((y, index) => ({ x: index * step, y }))
}

export function buildSmoothLinePath(points: SparklinePoint[]): string {
  if (points.length === 0) return ''
  const [first, ...rest] = points
  return [
    `M ${first.x.toFixed(2)} ${first.y.toFixed(2)}`,
    ...rest.map((point) => `L ${point.x.toFixed(2)} ${point.y.toFixed(2)}`),
  ].join(' ')
}

export function buildAreaPath(
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

export function buildSparklinePoints(
  series: number[],
  width: number,
  height: number
): SparklinePoint[] {
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
