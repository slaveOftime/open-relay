import { SPARKLINE_BUCKET_MS } from '@/lib/sparklineStore'

const BUCKET_SECONDS = SPARKLINE_BUCKET_MS / 1000
// Two seconds of output, including silence, so Recent falls back to zero promptly.
const RECENT_RATE_BUCKETS = 4

/** The newest time bucket is still open. Keep its visual endpoint level until
 * it receives bytes or the following bucket starts; rates still use raw data. */
export function carryOpenBucket(series: number[]): number[] {
  const last = series.length - 1
  if (last < 1 || series[last] !== 0 || series[last - 1] === 0) return series
  const display = series.slice()
  display[last] = series[last - 1]
  return display
}

/** Compute the newest point for a live update without rebuilding SVG path strings. */
export function calculateSparklineLastY(series: number[], height: number): number {
  const baselineY = Math.max(2, height - 3)
  if (series.length < 2) return baselineY
  let maxValue = 0
  for (const value of series) maxValue = Math.max(maxValue, value)
  const latestValue = series[series.length - 1] ?? 0
  const normalized = maxValue <= 0 ? 0 : Math.log10(latestValue + 1) / Math.log10(maxValue + 1)
  const emphasis = normalized <= 0 ? 0 : Math.pow(normalized, 0.86)
  const range = Math.max(height - 2 - 3, 1)
  return height - 3 - emphasis * range
}
export function calculateRecentBytesPerSecond(series: number[]): number {
  const recent = series.slice(-RECENT_RATE_BUCKETS)
  if (recent.length === 0) return 0
  return recent.reduce((sum, value) => sum + value, 0) / (recent.length * BUCKET_SECONDS)
}

export function calculatePeakBytesPerSecond(series: number[]): number {
  // One-second rolling maximum smooths bursts split across the 500 ms SSE
  // sampling boundary, instead of reporting a misleading instantaneous rate.
  let peak = 0
  for (let i = 0; i < series.length; i++) {
    peak = Math.max(peak, series[i] + (series[i - 1] ?? 0))
  }
  return peak / (2 * BUCKET_SECONDS)
}

export function calculateAverageBytesPerSecond(series: number[]): number {
  if (series.length === 0) return 0
  return series.reduce((sum, value) => sum + value, 0) / (series.length * BUCKET_SECONDS)
}

export function formatBytesPerSecond(value: number): string {
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
