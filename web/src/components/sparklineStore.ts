// Rolling activity history backing SparklineSvg. Lives in its own module
// (not the component file) so react-refresh can fast-refresh the component
// without sharing non-component exports.

const SPARKLINE_NUM_BUCKETS = 40
const SPARKLINE_BUCKET_MS = 2_000

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
