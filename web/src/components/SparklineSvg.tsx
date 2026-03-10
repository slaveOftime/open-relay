import { useEffect, useRef, useState } from 'react'

interface Props {
  series: number[]
  width?: number
  height?: number
  fullWidth?: boolean
  className?: string
  enableAnimation: boolean
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
  const svg = buildSparklineSvg(series, renderWidth, height, enableAnimation)
  const classes = fullWidth
    ? `block w-full align-middle ${className ?? ''}`.trim()
    : `inline-block align-middle ${className ?? ''}`.trim()
  return <span ref={hostRef} className={classes} dangerouslySetInnerHTML={{ __html: svg }} />
}

// ---------------------------------------------------------------------------
// Sparkline data
// ---------------------------------------------------------------------------

/** Rolling activity history using time-bucketed counts (bucketMs-wide slots). */
export class SparklineStore {
  private readonly numBuckets = 40
  private readonly bucketMs = 2_000 // 2 s per bucket → 80 s window

  private data = new Map<string, { counts: number[]; lastBucket: number }>()

  private nowBucket(): number {
    return Math.floor(Date.now() / this.bucketMs)
  }

  private getOrCreate(id: string) {
    let entry = this.data.get(id)
    if (!entry) {
      entry = { counts: new Array(this.numBuckets).fill(0), lastBucket: this.nowBucket() }
      this.data.set(id, entry)
    }
    return entry
  }

  /** Advance the ring buffer to the current bucket, zero-filling gaps. */
  private advance(entry: { counts: number[]; lastBucket: number }) {
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

/** Build an SVG sparkline polyline string from a series of values. */
function buildSparklineSvg(
  series: number[],
  width = 80,
  height = 22,
  enableAnimation: boolean
): string {
  if (series.length < 2) {
    return `<svg width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" xmlns="http://www.w3.org/2000/svg">
      <line x1="0" y1="${height / 2}" x2="${width}" y2="${height / 2}" stroke="#4ADE8066" stroke-width="1.25">
        ${enableAnimation ? '<animate attributeName="opacity" values="0.35;0.7;0.35" dur="2.2s" repeatCount="indefinite" />' : ''}
      </line>
    </svg>`
  }

  const max = Math.max(...series, 1)
  const step = width / (series.length - 1)
  const points = series.map((v, i) => {
    const x = i * step
    const y = height - (v / max) * (height - 4) - 2
    return { x, y }
  })

  const path = points.reduce((acc, p, i) => {
    if (i === 0) return `M ${p.x.toFixed(1)} ${p.y.toFixed(1)}`
    const p0 = points[i - 2] ?? points[i - 1]
    const p1 = points[i - 1]
    const p2 = p
    const p3 = points[i + 1] ?? p2
    const cp1x = p1.x + (p2.x - p0.x) / 6
    const cp1y = p1.y + (p2.y - p0.y) / 6
    const cp2x = p2.x - (p3.x - p1.x) / 6
    const cp2y = p2.y - (p3.y - p1.y) / 6
    return `${acc} C ${cp1x.toFixed(1)} ${cp1y.toFixed(1)} ${cp2x.toFixed(1)} ${cp2y.toFixed(1)} ${p2.x.toFixed(1)} ${p2.y.toFixed(1)}`
  }, '')

  const first = points[0]
  const last = points[points.length - 1]
  const areaPath = `${path} L ${last.x.toFixed(1)} ${height.toFixed(1)} L ${first.x.toFixed(1)} ${height.toFixed(1)} Z`

  return `<svg width="${width}" height="${height}" viewBox="0 0 ${width} ${height}"
      xmlns="http://www.w3.org/2000/svg" style="overflow:visible">
    <path d="${areaPath}" fill="#4ADE801A"/>
    <path d="${path}" fill="none" stroke="#4ADE8055" stroke-width="3.2" stroke-linecap="round"
      stroke-linejoin="round" style="filter:blur(1.2px)"/>
    <path d="${path}" fill="none" stroke="#4ADE80" stroke-width="1.7" stroke-linecap="round"
      stroke-linejoin="round" />
    <path d="${path}" fill="none" stroke="#BBF7D0" stroke-width="1.1" stroke-linecap="round"
      stroke-linejoin="round" pathLength="100" stroke-dasharray="16 120">
      ${enableAnimation ? '<animate attributeName="stroke-dashoffset" values="116;0" dur="2.4s" repeatCount="indefinite" />' : ''}
      ${enableAnimation ? '<animate attributeName="opacity" values="0.25;0.95;0.25" dur="2.4s" repeatCount="indefinite" />' : ''}
    </path>
    <circle cx="${last.x.toFixed(1)}" cy="${last.y.toFixed(1)}" r="1.8" fill="#86EFAC">
      ${enableAnimation ? '<animate attributeName="r" values="1.6;2.5;1.6" dur="1.6s" repeatCount="indefinite" />' : ''}
      ${enableAnimation ? '<animate attributeName="opacity" values="0.65;1;0.65" dur="1.6s" repeatCount="indefinite" />' : ''}
    </circle>
  </svg>`
}
