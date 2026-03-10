import { useState } from 'react'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'
import { SendIcon } from 'lucide-react'
import { parseKeySpec, parseKeyInputSpecs, splitKeyInput } from '@/utils/keyInput'
import {
  ArrowDownIcon,
  ArrowUpIcon,
  DoubleArrowDownIcon,
  DoubleArrowUpIcon,
} from '@radix-ui/react-icons'

// ── Input history ─────────────────────────────────────────────────────────────
const INPUT_HISTORY_KEY = 'open-relay:input-history'

interface InputHistoryEntry {
  text: string
  count: number
}

function loadInputHistory(): InputHistoryEntry[] {
  try {
    const raw = localStorage.getItem(INPUT_HISTORY_KEY)
    if (!raw) return []
    return JSON.parse(raw) as InputHistoryEntry[]
  } catch {
    return []
  }
}

function saveInputHistory(text: string): void {
  const trimmed = text.trim()
  if (!trimmed) return
  try {
    const history = loadInputHistory()
    const existing = history.find((e) => e.text === trimmed)
    if (existing) existing.count += 1
    else history.push({ text: trimmed, count: 1 })
    history.sort((a, b) => b.count - a.count)
    localStorage.setItem(INPUT_HISTORY_KEY, JSON.stringify(history.slice(0, 50)))
  } catch {
    /* ignore */
  }
}

// ── AttachPanel ───────────────────────────────────────────────────────────────
interface AttachPanelProps {
  sendInput: (data: string) => void
  showKeyError: (message: string) => void
}

const popularKeys = [
  { key: 'ctrl', label: 'ctrl', instant: false },
  { key: 'shift', label: 'shift', instant: false },
  { key: 'alt', label: 'alt', instant: false },
  { key: 'meta', label: 'meta', instant: false },
  { key: 'tab', label: 'tab', instant: false },
  { key: 'esc', label: 'esc', instant: true },
  { key: 'enter', label: 'enter', instant: true },
  { key: 'shift+tab', label: 'shift+tab', instant: true },
  { key: 'ctrl+c', label: '^C', instant: true },
  { key: 'ctrl+d', label: '^D', instant: true },
  { key: 'ctrl+l', label: '^L', instant: true },
  { key: 'ctrl+z', label: '^Z', instant: true },
  { key: 'left', label: '←', instant: true },
  { key: 'up', label: '↑', instant: true },
  { key: 'down', label: '↓', instant: true },
  { key: 'right', label: '→', instant: true },
  { key: 'backspace', label: '⌫', instant: true },
  { key: 'home', label: 'home', instant: true },
  { key: 'end', label: 'end', instant: true },
  { key: 'pgup', label: 'pgup', instant: true },
  { key: 'pgdn', label: 'pgdn', instant: true },
  { key: 'ins', label: 'ins', instant: true },
  { key: 'del', label: 'del', instant: true },
]

export default function AttachPanel({ sendInput, showKeyError }: AttachPanelProps) {
  const [drawerOpen, setDrawerOpen] = useState(false)
  const [customInput, setCustomInput] = useState('')
  const [customKeys, setCustomKeys] = useState('')
  const [inputHistory, setInputHistory] = useState<InputHistoryEntry[]>(() => loadInputHistory())

  function handleCustomSend() {
    if (!customInput) return
    saveInputHistory(customInput)
    setInputHistory(loadInputHistory())
    sendInput(customInput)
    setCustomInput('')
  }

  function handleSendKeySpec(spec: string) {
    try {
      sendInput(parseKeySpec(spec))
    } catch (error) {
      showKeyError(error instanceof Error ? error.message : 'invalid key spec')
    }
  }

  function handleSendCustomKeys(raw: string) {
    const specs = splitKeyInput(raw.trim())
    if (specs.length === 0) return
    try {
      const parsed = parseKeyInputSpecs(specs)
      for (const data of parsed) {
        sendInput(data)
      }
      setCustomKeys('')
    } catch (error) {
      showKeyError(error instanceof Error ? error.message : 'invalid key spec')
    }
  }

  function toggleDrawer() {
    setDrawerOpen(!drawerOpen)
    setTimeout(() => {
      const container = document.getElementById('main-container')
      if (container) {
        container.scrollTop = container.scrollHeight
      }
    }, 0)
  }

  const historyTop = [...inputHistory].sort((a, b) => b.count - a.count).slice(0, 15)

  return (
    <div>
      <div
        className={`${drawerOpen ? '' : 'h-0 sm:h-full sm:visible sm:w-[200px] md:w-[300px]'} overflow-hidden transition-all`}
      >
        <div className={`flex flex-col gap-4 p-3 `}>
          <div>
            <p className="text-xs text-[hsl(var(--muted-foreground))] font-medium mb-2">
              Text Input
            </p>
            <datalist id="or-input-history">
              {historyTop.map((e) => (
                <option key={e.text} value={e.text} />
              ))}
            </datalist>
            <div className="flex items-center gap-1">
              <Input
                className="text-sm flex-1 min-w-0"
                placeholder="Type text then press Enter"
                list="or-input-history"
                value={customInput}
                onChange={(e) => setCustomInput(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') {
                    e.preventDefault()
                    handleCustomSend()
                  }
                }}
              />
              <Tooltip>
                <TooltipContent>Send the input and press Enter</TooltipContent>
                <TooltipTrigger asChild>
                  <Button
                    variant="ghost"
                    disabled={!customInput.trim()}
                    onClick={() => {
                      setCustomInput(customInput)
                      handleCustomSend()
                      handleSendKeySpec('enter') // send an extra enter to trigger processing, since some inputs may be buffered until enter is pressed
                    }}
                  >
                    <svg
                      xmlns="http://www.w3.org/2000/svg"
                      fill="none"
                      viewBox="0 0 24 24"
                      stroke-width="1.5"
                      stroke="currentColor"
                      className="w-4 h-4"
                    >
                      <path
                        stroke-linecap="round"
                        stroke-linejoin="round"
                        d="m7.49 12-3.75 3.75m0 0 3.75 3.75m-3.75-3.75h16.5V4.499"
                      />
                    </svg>
                  </Button>
                </TooltipTrigger>
              </Tooltip>
            </div>
          </div>
          <div>
            <p className="text-xs text-[hsl(var(--muted-foreground))] font-medium mb-2">
              Quick Keys
            </p>
            <div className="grid grid-cols-4 gap-1.5 max-h-27 sm:max-h-fit overflow-y-auto">
              {popularKeys.map(({ key, label, instant }) => (
                <Tooltip key={key}>
                  <TooltipTrigger asChild>
                    <Button
                      type="button"
                      variant={instant ? 'default' : 'secondary'}
                      size="sm"
                      className="font-mono text-xs"
                      onClick={() => {
                        if (instant) {
                          if (customKeys.trim()) {
                            // compose with any pending modifier already in the queue
                            handleSendCustomKeys(`${customKeys.trim()} ${key}`)
                          } else {
                            handleSendKeySpec(key)
                          }
                          return
                        }
                        setCustomKeys((prev) =>
                          prev.trim() ? `${prev.trim()} ${key} ` : key + ' '
                        )
                        document.getElementById('custom-keys')?.focus()
                      }}
                    >
                      {label}
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent>{instant ? `${key} (instant)` : `${key} (queue)`}</TooltipContent>
                </Tooltip>
              ))}
            </div>
            <p className="mt-1 text-xs text-[hsl(var(--primary))]">
              ⚡colorful key will be sent immediately
            </p>
            <div className="mt-2 flex items-center gap-1">
              <Input
                id="custom-keys"
                className="text-sm"
                placeholder="Keys separated by whitespace"
                value={customKeys}
                onChange={(e) => setCustomKeys(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') {
                    e.preventDefault()
                    handleSendCustomKeys(customKeys)
                  }
                }}
              />
              <Button
                variant="ghost"
                disabled={!customKeys.trim()}
                onClick={() => handleSendCustomKeys(customKeys)}
              >
                <SendIcon className="h-4 w-4" />
              </Button>
            </div>
          </div>
        </div>
      </div>
      <div className="sm:hidden w-full h-10 flex justify-between items-center">
        <Button
          variant={'ghost'}
          className="text-[hsl(var(--primary))]"
          onClick={() => handleSendKeySpec('up')}
          aria-label="Up"
        >
          <ArrowUpIcon className="w-6 h-6" />
        </Button>
        <Button
          variant={'ghost'}
          className="text-[hsl(var(--primary))]"
          onClick={() => handleSendKeySpec('down')}
          aria-label="Down"
        >
          <ArrowDownIcon className="w-6 h-6" />
        </Button>
        <Button
          variant={'ghost'}
          className="text-[hsl(var(--primary))]"
          onClick={() => handleSendKeySpec('enter')}
          aria-label="Enter"
        >
          Enter
        </Button>
        <div className="flex-1 h-full" onClick={toggleDrawer}></div>
        <Button variant="ghost" onClick={toggleDrawer} aria-label="Open input panel">
          {drawerOpen ? (
            <DoubleArrowDownIcon className="w-6 h-6" />
          ) : (
            <DoubleArrowUpIcon className="w-6 h-6" />
          )}
        </Button>
      </div>
    </div>
  )
}
