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
  return d.toLocaleString([], {
    year: 'numeric',
    month: 'short',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
  })
}

export function formatByteSize(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return '0b'
  const units = ['b', 'kb', 'mb', 'gb', 'tb']
  let scaled = value
  let unitIndex = 0
  while (scaled >= 1024 && unitIndex < units.length - 1) {
    scaled /= 1024
    unitIndex += 1
  }
  const digits = scaled >= 100 || unitIndex === 0 ? 0 : scaled >= 10 ? 1 : 2
  return `${scaled.toFixed(digits)}${units[unitIndex]}`
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
    case 'killed':
      return 'dot-failed'
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
    case 'killed':
      return 'badge-failed'
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
    case 'killed':
      return 'Killed'
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

export function sessionDisplayName(s: { command: string; args: string[] }): string {
  return [s.command, ...s.args].map(shellQuote).join(' ')
}

export function sessionPrimaryLabel(s: {
  title?: string | null
  command: string
  args: string[]
}): string {
  const title = typeof s.title === 'string' ? s.title.trim() : ''
  return title || sessionDisplayName(s)
}

export function commandBaseName(command: string): string {
  const base = command.split(/[/\\]/).pop() ?? command
  return base.replace(/\.(?:exe|cmd|bat|ps1)$/i, '')
}

export function agentName(command: string): string {
  const base = commandBaseName(command)
  const known: Record<string, string> = {
    claude: 'Claude Code',
    'claude-code': 'Claude Code',
    gemini: 'Gemini CLI',
    'gemini-cli': 'Gemini CLI',
    powershell: 'PowerShell',
    pwsh: 'PowerShell',
    codex: 'Codex',
    opencode: 'OpenCode',
    qwen: 'Qwen',
    'qwen-code': 'Qwen',
    aider: 'Aider',
    bash: 'Bash',
    sh: 'Shell',
    zsh: 'Zsh',
    cmd: 'Command Prompt',
    node: 'Node.js',
    python: 'Python',
    python3: 'Python',
    copilot: 'GitHub Copilot',
  }
  return known[base.toLowerCase()] ?? base
}

export function cwdBasename(cwd: string | null): string {
  if (!cwd) return ''
  return cwd.replace(/\\/g, '/').split('/').filter(Boolean).pop() ?? cwd
}

// ── Shell arg parser ─────────────────────────────────────────────────────────
// Handles single/double quotes and backslash escapes, e.g.:
//   --model "claude 3.5" --flag 'hello world' --path C:\\foo
function isEscapableArgChar(ch: string | undefined): boolean {
  return ch !== undefined && /[\s"'\\]/.test(ch)
}

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
      } else if (ch === '\\' && (input[i + 1] === '"' || input[i + 1] === '\\')) {
        i++
        current += input[i]
      } else {
        current += ch
      }
    } else if (ch === "'") {
      inSingle = true
    } else if (ch === '"') {
      inDouble = true
    } else if (ch === '\\' && isEscapableArgChar(input[i + 1])) {
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
