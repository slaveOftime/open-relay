// Rolling activity history backing SparklineSvg. Lives in its own module
// (not the component file) so react-refresh can fast-refresh the component
// without sharing non-component exports.

const SPARKLINE_NUM_BUCKETS = 40
// The backend polls output summaries every 500 ms. Match that cadence
// without increasing SSE traffic or work done on every React render.
export const SPARKLINE_BUCKET_MS = 500

export type SparklineActivitySnapshot = {
  series: number[]
  bucketIndex: number
  lastOutputAt: number | null
}

type Entry = {
  counts: number[]
  snapshot: number[]
  lastBucket: number
  lastTotalBytes: number | null
  lastOutputAt: number | null
  activitySnapshot: SparklineActivitySnapshot
}

/** Rolling output-byte history, with notifications scoped to the affected session. */
export class SparklineStore {
  private readonly numBuckets = SPARKLINE_NUM_BUCKETS
  private readonly bucketMs = SPARKLINE_BUCKET_MS
  private readonly listeners = new Map<string, Set<() => void>>()
  private readonly emptySeries = new Array(this.numBuckets).fill(0)
  private readonly emptyActivitySnapshot: SparklineActivitySnapshot = {
    series: this.emptySeries,
    bucketIndex: 0,
    lastOutputAt: null,
  }
  private decayTimer: ReturnType<typeof setInterval> | null = null
  private data = new Map<string, Entry>()

  private nowBucket(): number {
    return Math.floor(Date.now() / this.bucketMs)
  }

  private getOrCreate(id: string): Entry {
    let entry = this.data.get(id)
    if (!entry) {
      const counts = new Array(this.numBuckets).fill(0)
      const lastBucket = this.nowBucket()
      const snapshot = [...counts]
      entry = {
        counts,
        snapshot,
        lastBucket,
        lastTotalBytes: null,
        lastOutputAt: null,
        activitySnapshot: { series: snapshot, bucketIndex: lastBucket, lastOutputAt: null },
      }
      this.data.set(id, entry)
    }
    return entry
  }

  subscribe(id: string, listener: () => void): () => void {
    let listeners = this.listeners.get(id)
    if (!listeners) {
      listeners = new Set()
      this.listeners.set(id, listeners)
    }
    listeners.add(listener)
    // An unmounted/hidden graph may have missed several buckets.
    const entry = this.data.get(id)
    if (entry && this.advance(entry)) this.emitChange(id)
    if (entry?.counts.some((value) => value !== 0)) this.ensureDecayTimer()
    return () => {
      listeners.delete(listener)
      if (listeners.size === 0) this.listeners.delete(id)
      // Intentionally keep the decay timer running: row data persists across
      // page navigations and SSE-driven records must keep bucket windows
      // advancing while no UI is mounted. Stopping here wiped visible history
      // for users that briefly navigated away (more than 20 s away caused
      // every bucket to expire before the next subscribe could advance).
    }
  }

  private emitChange(id: string): void {
    this.listeners.get(id)?.forEach((listener) => listener())
  }

  private ensureDecayTimer(): void {
    if (this.decayTimer !== null || typeof window === 'undefined') return
    // One timer only while there is any history worth aging. Crucially, we
    // iterate `this.data` (not `this.listeners`): even if no UI is mounted
    // we still advance bucket windows so that on remount we don't see a
    // giant gap that flips the visible series to all zeros.
    this.decayTimer = window.setInterval(() => {
      let active = false
      for (const [id, entry] of this.data) {
        if (entry.counts.some((value) => value !== 0)) active = true
        if (!this.advance(entry)) continue
        this.listeners.get(id)?.forEach((listener) => listener())
      }
      if (!active) this.stopDecayTimer()
    }, this.bucketMs)
  }

  private stopDecayTimer(): void {
    if (this.decayTimer === null) return
    clearInterval(this.decayTimer)
    this.decayTimer = null
  }

  /** Shift expired buckets; an all-zero history needs no copy or repaint. */
  private advance(entry: Entry): boolean {
    const now = this.nowBucket()
    const delta = now - entry.lastBucket
    if (delta <= 0) return false
    const gap = Math.min(delta, this.numBuckets)
    entry.lastBucket = now
    if (!entry.counts.some((value) => value !== 0)) return false

    if (gap === this.numBuckets) {
      entry.counts.fill(0)
    } else {
      for (let i = 0; i < gap; i++) {
        entry.counts.shift()
        entry.counts.push(0)
      }
    }
    entry.snapshot = [...entry.counts]
    entry.activitySnapshot = {
      series: entry.snapshot,
      bucketIndex: now,
      lastOutputAt: entry.lastOutputAt,
    }
    return true
  }

  ensure(id: string): void {
    this.getOrCreate(id)
  }

  /** Increment the current time bucket for this session. */
  touch(id: string, value = 1): void {
    const entry = this.getOrCreate(id)
    const advanced = this.advance(entry)
    if (value > 0) {
      entry.counts[entry.counts.length - 1] += value
      entry.snapshot = [...entry.counts]
      entry.activitySnapshot = {
        series: entry.snapshot,
        bucketIndex: entry.lastBucket,
        lastOutputAt: entry.lastOutputAt,
      }
    }
    if (value > 0) this.ensureDecayTimer()
    if (advanced || value > 0) this.emitChange(id)
  }

  /** Record absolute byte totals and add only new bytes to the current bucket. */
  recordTotal(id: string, totalBytes: number, previousTotalBytes?: number): void {
    if (!Number.isFinite(totalBytes) || totalBytes < 0) return
    const entry = this.getOrCreate(id)
    const advanced = this.advance(entry)
    // An older REST response or SSE event must not roll the baseline back and
    // manufacture a spike when the next current total arrives.
    const baseline = Math.max(
      entry.lastTotalBytes ?? previousTotalBytes ?? totalBytes,
      previousTotalBytes ?? 0
    )
    const delta = Math.max(totalBytes - baseline, 0)
    entry.lastTotalBytes = Math.max(entry.lastTotalBytes ?? totalBytes, totalBytes)
    if (delta > 0) {
      entry.lastOutputAt = Date.now()
      entry.counts[entry.counts.length - 1] += delta
      entry.snapshot = [...entry.counts]
      entry.activitySnapshot = {
        series: entry.snapshot,
        bucketIndex: entry.lastBucket,
        lastOutputAt: entry.lastOutputAt,
      }
    }
    if (delta > 0) this.ensureDecayTimer()
    if (advanced || delta > 0) this.emitChange(id)
  }

  getActivitySnapshot(id: string): SparklineActivitySnapshot {
    return this.data.get(id)?.activitySnapshot ?? this.emptyActivitySnapshot
  }

  getBucketIndex(id: string): number {
    return this.data.get(id)?.lastBucket ?? 0
  }

  getLastOutputAt(id: string): number | null {
    return this.data.get(id)?.lastOutputAt ?? null
  }
  /** A stable, side-effect-free snapshot for useSyncExternalStore. */
  getSeries(id: string): number[] {
    return this.data.get(id)?.snapshot ?? this.emptySeries
  }

  remove(id: string): void {
    if (this.data.delete(id)) this.emitChange(id)
  }
}
