import type { SessionStatus } from '../api/types.ts'

// ---------------------------------------------------------------------------
// Age / time formatting
// ---------------------------------------------------------------------------

export function formatAge(createdAt: string): string {
  const diff = Math.floor((Date.now() - new Date(createdAt).getTime()) / 1000)
  if (diff < 60) return `${diff}s ago`
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`
  return `${Math.floor(diff / 86400)}d ago`
}

export function formatTimestamp(iso: string): string {
  const d = new Date(iso)
  return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
}

// ---------------------------------------------------------------------------
// Status helpers
// ---------------------------------------------------------------------------

export function statusDotClass(status: SessionStatus): string {
  switch (status) {
    case 'running':
      return 'dot-running'
    case 'stopping':
      return 'dot-stopping'
    case 'failed':
      return 'dot-failed'
    default:
      return 'dot-stopped'
  }
}

export function statusBadgeClass(status: SessionStatus, inputNeeded: boolean): string {
  if (inputNeeded) return 'badge-input-needed'
  switch (status) {
    case 'running':
      return 'badge-running'
    case 'stopping':
      return 'badge-stopping'
    case 'failed':
      return 'badge-failed'
    default:
      return 'badge-stopped'
  }
}

export function statusLabel(status: SessionStatus, inputNeeded: boolean): string {
  if (inputNeeded) return 'Input Needed'
  switch (status) {
    case 'running':
      return 'Running'
    case 'stopping':
      return 'Stopping'
    case 'stopped':
      return 'Stopped'
    case 'failed':
      return 'Failed'
    case 'created':
      return 'Created'
  }
}

// ---------------------------------------------------------------------------
// Title / display name
// ---------------------------------------------------------------------------

/** Quote a single shell token if it contains whitespace or shell-special chars. */
function shellQuote(token: string): string {
  if (/[\s"'\\$`!|&;<>(){}]/.test(token)) {
    // Escape inner double-quotes and wrap in double quotes
    return '"' + token.replace(/\\/g, '\\\\').replace(/"/g, '\\"') + '"'
  }
  return token
}

export function sessionDisplayName(s: {
  command: string
  args: string[]
}): string {
  return [s.command, ...s.args].map(shellQuote).join(' ').slice(0, 64)
}

export function agentName(command: string): string {
  const base = command.split(/[/\\]/).pop() ?? command
  const known: Record<string, string> = {
    claude: 'Claude Code',
    'claude-code': 'Claude Code',
    gemini: 'Gemini-CLI',
    'gemini-cli': 'Gemini-CLI',
    aider: 'Aider v0.2',
    bash: 'Shell',
    sh: 'Shell',
    zsh: 'Shell',
    cmd: 'Shell',
    node: 'Node.js',
    python: 'Python',
    python3: 'Python',
  }
  return known[base.toLowerCase()] ?? base
}

export function cwdBasename(cwd: string | null): string {
  if (!cwd) return ''
  return cwd.replace(/\\/g, '/').split('/').filter(Boolean).pop() ?? cwd
}

// ---------------------------------------------------------------------------
// Sparkline data
// ---------------------------------------------------------------------------

/** Rolling activity history using time-bucketed counts (bucketMs-wide slots). */
export class SparklineStore {
  private readonly numBuckets = 20
  private readonly bucketMs = 2_000 // 2 s per bucket → 40 s window

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
export function buildSparklineSvg(series: number[], width = 80, height = 22): string {
  if (series.length < 2) {
    return `<svg width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" xmlns="http://www.w3.org/2000/svg">
      <line x1="0" y1="${height / 2}" x2="${width}" y2="${height / 2}" stroke="#374151" stroke-width="1"/>
    </svg>`
  }

  const max = Math.max(...series, 1)
  const step = width / (series.length - 1)
  const pts = series
    .map((v, i) => {
      const x = i * step
      const y = height - (v / max) * (height - 4) - 2
      return `${x.toFixed(1)},${y.toFixed(1)}`
    })
    .join(' ')

  return `<svg width="${width}" height="${height}" viewBox="0 0 ${width} ${height}"
      xmlns="http://www.w3.org/2000/svg" style="overflow:visible">
    <polyline points="${pts}" fill="none" stroke="#4ADE80" stroke-width="1.5"
      stroke-linejoin="round" stroke-linecap="round"
      style="filter:drop-shadow(0 0 3px #4ADE8088)"/>
  </svg>`
}


// ── Shell arg parser ─────────────────────────────────────────────────────────
// Handles single/double quotes and backslash escapes, e.g.:
//   --model "claude 3.5" --flag 'hello world' --path C:\\foo
export function parseArgString(input: string): string[] {
  const args: string[] = []
  let current = ''
  let inSingle = false
  let inDouble = false
  let i = 0
  while (i < input.length) {
    const ch = input[i]
    if (inSingle) {
      if (ch === "'") {
        inSingle = false
      } else {
        current += ch
      }
    } else if (inDouble) {
      if (ch === '"') {
        inDouble = false
      } else if (ch === '\\' && i + 1 < input.length) {
        i++
        current += input[i]
      } else {
        current += ch
      }
    } else if (ch === "'") {
      inSingle = true
    } else if (ch === '"') {
      inDouble = true
    } else if (ch === '\\' && i + 1 < input.length) {
      i++
      current += input[i]
    } else if (/\s/.test(ch)) {
      if (current.length > 0) {
        args.push(current)
        current = ''
      }
    } else {
      current += ch
    }
    i++
  }
  if (current.length > 0) args.push(current)
  return args
}