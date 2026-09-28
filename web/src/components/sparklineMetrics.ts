import { SPARKLINE_BUCKET_MS } from './sparklineStore'

const BUCKET_SECONDS = SPARKLINE_BUCKET_MS / 1000
// Two seconds of output, including silence, so Recent falls back to zero promptly.
const RECENT_RATE_BUCKETS = 4

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
