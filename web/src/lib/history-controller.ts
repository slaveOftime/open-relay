/**
 * HistoryController — the browser-side owner of paginated session history
 * (M4-2, PLAN §9.2).
 *
 * Extracted from SessionDetailPage so the paging/anchoring policy is a pure,
 * testable state machine instead of a web of refs. The xterm-facing replay
 * effects stay in the page; this class owns:
 *
 * - the loaded chunk list and resize records,
 * - the server-anchored page offset (`loadedCount`),
 * - in-flight fetch de-duplication,
 * - generation-based cancellation (a `reset()` invalidates any in-flight
 *   page so a stale response can never append duplicates),
 * - loud anchor mismatch: if the server answers a page with an offset other
 *   than the one requested, we throw instead of silently duplicating or
 *   truncating retained history.
 */

export interface LogResizeRecord {
  offset: number
  rows: number
  cols: number
}

export interface HistoryPage {
  /** Server-confirmed start offset of `chunks`; must equal the request. */
  offset: number
  chunks: Uint8Array[]
  total: number
  resizes: LogResizeRecord[]
}

export type HistoryFetcher = (offset: number, limit: number) => Promise<HistoryPage>

export class HistoryAnchorError extends Error {
  readonly requested: number
  readonly received: number

  constructor(requested: number, received: number) {
    super(
      `history page anchor mismatch: requested offset ${requested}, server returned ${received}`
    )
    this.name = 'HistoryAnchorError'
    this.requested = requested
    this.received = received
  }
}

export class HistoryController {
  private chunks: Uint8Array[] = []
  private resizes: LogResizeRecord[] = []
  private total = 0
  private loadedCount = 0
  private fetching = false
  private generation = 0

  private readonly fetchPage: HistoryFetcher
  private readonly pageSize: number

  constructor(fetchPage: HistoryFetcher, pageSize: number) {
    this.fetchPage = fetchPage
    this.pageSize = pageSize
  }

  get chunkCount(): number {
    return this.chunks.length
  }

  get totalChunks(): number {
    return this.total
  }

  get loaded(): number {
    return this.loadedCount
  }

  get isFetching(): boolean {
    return this.fetching
  }

  /** More pages may exist. `total === 0` means "not yet known". */
  get hasMore(): boolean {
    return this.total === 0 || this.loadedCount < this.total
  }

  getChunks(): readonly Uint8Array[] {
    return this.chunks
  }

  getResizes(): readonly LogResizeRecord[] {
    return this.resizes
  }

  /**
   * Drop all loaded state and invalidate any in-flight fetch. The stale
   * response is discarded when it resolves (generation check).
   */
  reset(): void {
    this.generation += 1
    this.chunks = []
    this.resizes = []
    this.total = 0
    this.loadedCount = 0
    // The new generation is not fetching; the stale in-flight fetch's
    // finally block skips the flag because its generation no longer matches.
    this.fetching = false
  }

  /** Clamp a replay index into the loaded range. */
  clampIndex(index: number): number {
    return Math.max(0, Math.min(index, this.chunks.length))
  }

  /** Initial load from offset 0, replacing any previously loaded state. */
  async loadInitial(): Promise<number> {
    this.reset()
    return this.fetchAnchored(0)
  }

  /**
   * Append the next page, anchored at the current loaded offset. Concurrent
   * calls collapse to the in-flight one (no duplicate fetches). Returns the
   * number of newly loaded chunks (0 = nothing new).
   *
   * @throws HistoryAnchorError if the server answers with a different offset
   *         than requested — retained history must never silently duplicate
   *         or truncate.
   */
  async loadMore(): Promise<number> {
    if (this.fetching) return 0
    if (!this.hasMore) return 0
    return this.fetchAnchored(this.loadedCount)
  }

  private async fetchAnchored(offset: number): Promise<number> {
    const generation = this.generation
    this.fetching = true
    try {
      const page = await this.fetchPage(offset, this.pageSize)
      if (generation !== this.generation) {
        // A reset() happened while this page was in flight: discard it.
        return 0
      }
      if (page.offset !== offset) {
        throw new HistoryAnchorError(offset, page.offset)
      }
      this.resizes = page.resizes
      if (page.chunks.length > 0) {
        this.chunks = [...this.chunks, ...page.chunks]
        this.loadedCount = page.offset + page.chunks.length
      }
      this.total = page.total
      return page.chunks.length
    } finally {
      if (generation === this.generation) {
        this.fetching = false
      }
    }
  }
}
