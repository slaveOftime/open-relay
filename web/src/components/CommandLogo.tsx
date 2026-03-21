import { cn } from '@/lib/utils'
import { agentName, commandBaseName } from '@/utils/format'

type CommandBrand =
  | 'anthropic'
  | 'bash'
  | 'claude'
  | 'cmd'
  | 'codex'
  | 'gemini'
  | 'generic'
  | 'node'
  | 'opencode'
  | 'powershell'
  | 'python'
  | 'qwen'
  | 'shell'
  | 'aider'
  | 'copilot'
  | 'ollama'

type CommandVisual = {
  src?: string
  monochrome?: boolean
  fallbackText: string
  fallbackClassName: string
}

const COMMAND_ALIASES: Record<string, CommandBrand> = {
  anthropic: 'anthropic',
  aider: 'aider',
  bash: 'bash',
  claude: 'claude',
  'claude-code': 'claude',
  cmd: 'cmd',
  codex: 'codex',
  gemini: 'gemini',
  'gemini-cli': 'gemini',
  node: 'node',
  nodejs: 'node',
  opencode: 'opencode',
  powershell: 'powershell',
  pwsh: 'powershell',
  python: 'python',
  python3: 'python',
  qwen: 'qwen',
  'qwen-code': 'qwen',
  'qwen-coder': 'qwen',
  sh: 'shell',
  zsh: 'shell',
  fish: 'shell',
  copilot: 'copilot',
  'github-copilot': 'copilot',
  ollama: 'ollama',
}

const COMMAND_VISUALS: Record<CommandBrand, CommandVisual> = {
  anthropic: {
    src: '/command-logos/anthropic.svg',
    monochrome: true,
    fallbackText: 'AN',
    fallbackClassName: 'bg-[#c28563]/15 text-[#c28563]',
  },
  aider: {
    fallbackText: 'AI',
    fallbackClassName: 'bg-[#0ea5e9]/15 text-[#38bdf8]',
  },
  bash: {
    src: '/command-logos/bash.svg',
    monochrome: true,
    fallbackText: 'B$',
    fallbackClassName: 'bg-[#4eaa25]/15 text-[#4eaa25]',
  },
  claude: {
    src: '/command-logos/claude.svg',
    monochrome: true,
    fallbackText: 'CC',
    fallbackClassName: 'bg-[#d97757]/15 text-[#d97757]',
  },
  cmd: {
    src: '/command-logos/windows.svg',
    fallbackText: 'C>',
    fallbackClassName: 'bg-[#0078d4]/15 text-[#38bdf8]',
  },
  codex: {
    fallbackText: 'CX',
    fallbackClassName: 'bg-[#10b981]/15 text-[#34d399]',
  },
  gemini: {
    src: '/command-logos/gemini.svg',
    monochrome: true,
    fallbackText: 'GM',
    fallbackClassName: 'bg-[#8b5cf6]/15 text-[#a78bfa]',
  },
  generic: {
    fallbackText: '?>',
    fallbackClassName: 'bg-[hsl(var(--muted))] text-[hsl(var(--muted-foreground))]',
  },
  node: {
    src: '/command-logos/node.svg',
    fallbackText: 'JS',
    fallbackClassName: 'bg-[#3c873a]/15 text-[#65a30d]',
  },
  opencode: {
    src: '/command-logos/opencode.ico',
    fallbackText: 'OC',
    fallbackClassName: 'bg-[#f97316]/15 text-[#fb923c]',
  },
  powershell: {
    src: '/command-logos/powershell.svg',
    fallbackText: 'PS',
    fallbackClassName: 'bg-[#0078d4]/15 text-[#38bdf8]',
  },
  python: {
    src: '/command-logos/python.ico',
    fallbackText: 'PY',
    fallbackClassName: 'bg-[#3776ab]/15 text-[#60a5fa]',
  },
  qwen: {
    src: '/command-logos/qwen.ico',
    fallbackText: 'QW',
    fallbackClassName: 'bg-[#ef4444]/15 text-[#f87171]',
  },
  shell: {
    src: '/command-logos/bash.svg',
    fallbackText: '$>',
    fallbackClassName: 'bg-[hsl(var(--muted))] text-[hsl(var(--foreground))]',
  },
  copilot: {
    src: '/command-logos/copilot.png',
    fallbackText: 'CP',
    fallbackClassName: 'bg-[#0ea5e9]/15 text-[#38bdf8]',
  },
  ollama: {
    src: '/command-logos/ollama.png',
    fallbackText: 'OL',
    fallbackClassName: 'bg-[#0ea5e9]/15 text-[#38bdf8]',
  },
}

function resolveCommandBrand(command: string): CommandBrand {
  const normalized = commandBaseName(command).toLowerCase()
  return COMMAND_ALIASES[normalized] ?? 'generic'
}

function fallbackMonogram(label: string): string {
  const words = label
    .replace(/[^a-zA-Z0-9]+/g, ' ')
    .trim()
    .split(/\s+/)
    .filter(Boolean)

  if (words.length === 0) return '?>'
  if (words.length === 1) return words[0].slice(0, 2).toUpperCase()
  return `${words[0][0] ?? ''}${words[1][0] ?? ''}`.toUpperCase()
}

export default function CommandLogo({
  command,
  size = 24,
  className,
}: {
  command: string
  size?: number
  className?: string
}) {
  const label = agentName(command)
  const brand = resolveCommandBrand(command)
  const visual = COMMAND_VISUALS[brand]
  const fallbackText = brand === 'generic' ? fallbackMonogram(label) : visual.fallbackText

  return (
    <span
      className={cn(
        'inline-flex shrink-0 items-center justify-center overflow-hidden rounded-md border border-[hsl(var(--border))] bg-[hsl(var(--background))] shadow-sm',
        className
      )}
      style={{ width: size, height: size }}
      aria-hidden="true"
    >
      {visual.src ? (
        <img
          src={visual.src}
          alt=""
          className={cn(
            'h-full w-full object-contain p-[2px]',
            visual.monochrome && 'dark:invert'
          )}
        />
      ) : (
        <span
          className={cn(
            'flex h-full w-full items-center justify-center text-[9px] font-semibold tracking-tight',
            visual.fallbackClassName
          )}
        >
          {fallbackText}
        </span>
      )}
    </span>
  )
}
