import { useCallback, useEffect, useRef, useState, type ChangeEvent } from 'react'
import { Button } from '@/components/ui/button'
import { FileDropZone } from './ui/file-drop-zone'
import { getFirstTransferredFile } from './ui/file-transfer'
import { Input } from '@/components/ui/input'
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip'
import { ArrowLeftIcon, ArrowRightIcon, PaperclipIcon, SendIcon } from 'lucide-react'
import { parseKeySpec, parseKeyInputSpecs, splitKeyInput } from '@/utils/keyInput'
import {
  ArrowDownIcon,
  ArrowUpIcon,
  DoubleArrowDownIcon,
  DoubleArrowUpIcon,
} from '@radix-ui/react-icons'
import type { UploadSessionFileResponse } from '@/api/client'

// ── Input history ─────────────────────────────────────────────────────────────
const INPUT_HISTORY_KEY = 'open-relay:input-history'
const SESSION_INPUT_DRAFT_KEY_PREFIX = 'open-relay:session-input-draft:'
const ATTACH_BUSY_INTERVAL_MS = 2000

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

function getSessionInputDraftKey(sessionId: string): string | null {
  const trimmed = sessionId.trim()
  return trimmed ? `${SESSION_INPUT_DRAFT_KEY_PREFIX}${trimmed}` : null
}

function loadSessionInputDraft(sessionId: string): string {
  const storageKey = getSessionInputDraftKey(sessionId)
  if (!storageKey) return ''
  try {
    return localStorage.getItem(storageKey) ?? ''
  } catch {
    return ''
  }
}

function saveSessionInputDraft(sessionId: string, text: string): void {
  const storageKey = getSessionInputDraftKey(sessionId)
  if (!storageKey) return
  try {
    if (text.length === 0) {
      localStorage.removeItem(storageKey)
      return
    }
    localStorage.setItem(storageKey, text)
  } catch {
    /* ignore */
  }
}

// ── AttachPanel ───────────────────────────────────────────────────────────────
interface AttachPanelProps {
  sessionId: string
  sendInput: (data: string) => void
  sendBusy: () => void
  showKeyError: (message: string) => void
  uploadFile?: (file: File) => Promise<UploadSessionFileResponse>
}

const popularKeys = [
  { key: 'ctrl', label: 'ctrl', instant: false },
  { key: 'shift', label: 'shift', instant: false },
  { key: 'alt', label: 'alt', instant: false },
  { key: 'meta', label: 'meta', instant: false },
  { key: 'tab', label: 'tab', instant: true },
  { key: 'shift+tab', label: 'shift+tab', instant: true },
  { key: 'esc', label: 'esc', instant: true },
  { key: 'enter', label: 'enter', instant: true },
  { key: 'ctrl+d', label: '^D', instant: true },
  { key: 'ctrl+l', label: '^L', instant: true },
  { key: 'ctrl+z', label: '^Z', instant: true },
  { key: 'ctrl+c', label: '^C', instant: true },
  { key: 'del', label: 'del', instant: true },
  { key: 'backspace', label: '⌫', instant: true },
  { key: 'home', label: 'home', instant: true },
  { key: 'end', label: 'end', instant: true },
  { key: 'left', label: '←', instant: true },
  { key: 'up', label: '↑', instant: true },
  { key: 'down', label: '↓', instant: true },
  { key: 'right', label: '→', instant: true },
  { key: 'pgup', label: 'pgup', instant: true },
  { key: 'pgdn', label: 'pgdn', instant: true },
  { key: 'ins', label: 'ins', instant: true },
]

export default function AttachPanel({
  sessionId,
  sendInput,
  sendBusy,
  showKeyError,
  uploadFile,
}: AttachPanelProps) {
  const [drawerOpen, setDrawerOpen] = useState(false)
  const [customInput, setCustomInput] = useState('')
  const [customKeys, setCustomKeys] = useState('')
  const [isUploading, setIsUploading] = useState(false)
  const rootRef = useRef<HTMLDivElement | null>(null)
  const customInputRef = useRef<HTMLTextAreaElement | null>(null)
  const fileInputRef = useRef<HTMLInputElement | null>(null)
  const sendClickTimeoutRef = useRef<number | null>(null)
  const shouldPersistDraftRef = useRef(false)
  const busyIntervalRef = useRef<number | null>(null)
  const drawerScrollTimeoutsRef = useRef<number[]>([])

  function findScrollContainer(node: HTMLElement | null): HTMLElement | null {
    let current = node?.parentElement ?? null
    while (current) {
      const style = window.getComputedStyle(current)
      const overflowY = style.overflowY
      const canScroll =
        (overflowY === 'auto' || overflowY === 'scroll') &&
        current.scrollHeight > current.clientHeight
      if (canScroll) {
        return current
      }
      current = current.parentElement
    }

    return document.scrollingElement instanceof HTMLElement
      ? document.scrollingElement
      : document.documentElement
  }

  function scrollDrawerIntoView() {
    const target = findScrollContainer(rootRef.current)
    if (!target) return
    target.scrollTo({ top: target.scrollHeight, behavior: 'auto' })
  }

  function resizeCustomInput() {
    const textarea = customInputRef.current
    if (!textarea) return

    textarea.style.height = 'auto'

    const computedStyle = window.getComputedStyle(textarea)
    const lineHeight = Number.parseFloat(computedStyle.lineHeight) || 20
    const paddingHeight =
      Number.parseFloat(computedStyle.paddingTop) + Number.parseFloat(computedStyle.paddingBottom)
    const borderHeight =
      Number.parseFloat(computedStyle.borderTopWidth) +
      Number.parseFloat(computedStyle.borderBottomWidth)
    const maxHeight = lineHeight * 8 + paddingHeight + borderHeight
    const nextHeight = Math.min(textarea.scrollHeight, maxHeight)

    textarea.style.height = `${nextHeight}px`
    textarea.style.overflowY = textarea.scrollHeight > maxHeight ? 'auto' : 'hidden'
  }

  useEffect(() => {
    resizeCustomInput()
  }, [customInput])

  useEffect(() => {
    shouldPersistDraftRef.current = false
    setCustomInput(loadSessionInputDraft(sessionId))
  }, [sessionId])

  useEffect(() => {
    if (!shouldPersistDraftRef.current) {
      shouldPersistDraftRef.current = true
      return
    }
    saveSessionInputDraft(sessionId, customInput)
  }, [customInput, sessionId])

  useEffect(() => {
    return () => {
      if (sendClickTimeoutRef.current !== null) {
        window.clearTimeout(sendClickTimeoutRef.current)
      }
      if (busyIntervalRef.current !== null) {
        window.clearInterval(busyIntervalRef.current)
      }
      for (const timeoutId of drawerScrollTimeoutsRef.current) {
        window.clearTimeout(timeoutId)
      }
      drawerScrollTimeoutsRef.current = []
    }
  }, [])

  useEffect(() => {
    if (busyIntervalRef.current !== null) {
      window.clearInterval(busyIntervalRef.current)
      busyIntervalRef.current = null
    }
  }, [sessionId])

  function handleCustomSend(sendEnter = false) {
    if (!customInput.trim()) return
    saveInputHistory(customInput)
    sendInput(customInput)
    if (sendEnter) {
      handleSendKeySpec('enter')
    }
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
    const nextOpen = !drawerOpen
    setDrawerOpen(nextOpen)

    for (const timeoutId of drawerScrollTimeoutsRef.current) {
      window.clearTimeout(timeoutId)
    }
    drawerScrollTimeoutsRef.current = []

    if (!nextOpen) return

    for (const delay of [0, 160, 320]) {
      const timeoutId = window.setTimeout(() => {
        scrollDrawerIntoView()
      }, delay)
      drawerScrollTimeoutsRef.current.push(timeoutId)
    }
  }

  function clearPendingSingleClick() {
    if (sendClickTimeoutRef.current !== null) {
      window.clearTimeout(sendClickTimeoutRef.current)
      sendClickTimeoutRef.current = null
    }
  }

  function handleSendButtonClick() {
    clearPendingSingleClick()
    sendClickTimeoutRef.current = window.setTimeout(() => {
      handleCustomSend(false)
      sendClickTimeoutRef.current = null
    }, 250)
  }

  function handleSendButtonDoubleClick() {
    clearPendingSingleClick()
    handleCustomSend(true)
  }

  function startBusyHeartbeat() {
    sendBusy()
    if (busyIntervalRef.current !== null) {
      window.clearInterval(busyIntervalRef.current)
    }
    busyIntervalRef.current = window.setInterval(() => {
      sendBusy()
    }, ATTACH_BUSY_INTERVAL_MS)
  }

  function stopBusyHeartbeat() {
    if (busyIntervalRef.current !== null) {
      window.clearInterval(busyIntervalRef.current)
      busyIntervalRef.current = null
    }
  }

  function handleUploadButtonClick() {
    if (!uploadFile || isUploading) return
    fileInputRef.current?.click()
  }

  const uploadSelectedFile = useCallback(
    async (file: File) => {
      if (!uploadFile) return

      setIsUploading(true)
      try {
        const response = await uploadFile(file)
        if (response.ok) {
          setCustomInput((prev) => {
            if (prev.trim()) {
              return prev + ' ' + response.path
            }
            return response.path
          })
        }
      } catch (error) {
        showKeyError(error instanceof Error ? error.message : 'file upload failed')
      } finally {
        setIsUploading(false)
      }
    },
    [showKeyError, uploadFile]
  )

  useEffect(() => {
    if (!uploadFile) return

    function handlePaste(event: ClipboardEvent) {
      if (isUploading) return
      const file = getFirstTransferredFile(event.clipboardData)
      if (!file) return
      event.preventDefault()
      void uploadSelectedFile(file)
    }

    window.addEventListener('paste', handlePaste)
    return () => {
      window.removeEventListener('paste', handlePaste)
    }
  }, [isUploading, uploadFile, uploadSelectedFile])

  async function handleUploadDrop(file: File) {
    await uploadSelectedFile(file)
  }

  async function handleFileInputChange(event: ChangeEvent<HTMLInputElement>) {
    const file = event.target.files?.[0]
    event.target.value = ''
    if (!file || !uploadFile) return

    await uploadSelectedFile(file)
  }

  return (
    <div ref={rootRef}>
      <div
        className={`${drawerOpen ? '' : 'h-0 sm:h-full sm:visible sm:w-[200px] md:w-[300px]'} overflow-hidden transition-all`}
      >
        <div className={`flex flex-col gap-4 p-3 `}>
          <div>
            <p className="text-xs text-[hsl(var(--muted-foreground))] font-medium mb-2">
              Text Input
            </p>
            <div className="relative">
              <textarea
                ref={customInputRef}
                className="flex min-h-[72px] w-full rounded-md border border-[hsl(var(--input))] bg-[hsl(var(--secondary))] px-3 py-2 pr-12 text-sm text-[hsl(var(--foreground))] placeholder:text-[hsl(var(--muted-foreground))] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[hsl(var(--ring))] focus-visible:ring-offset-1 disabled:cursor-not-allowed disabled:opacity-50 transition-colors resize-none"
                placeholder="Type text here. Enter adds a new line. ctrl+enter to send."
                rows={3}
                value={customInput}
                onChange={(e) => setCustomInput(e.target.value)}
                onFocus={startBusyHeartbeat}
                onBlur={stopBusyHeartbeat}
                onKeyDown={(e) => {
                  if (e.key === 'Enter' && e.ctrlKey) {
                    e.preventDefault()
                    // blur to stop busy heartbeat, otherwise it will keep sending busy signals every 2 seconds
                    e.currentTarget.blur()
                    handleCustomSend(true)
                  }
                }}
              />
              <Tooltip>
                <TooltipContent>
                  Single click sends text. Double click sends text and Enter.
                </TooltipContent>
                <TooltipTrigger asChild>
                  <Button
                    type="button"
                    variant="ghost"
                    className="absolute bottom-2 right-2 p-1"
                    disabled={!customInput.trim()}
                    onClick={handleSendButtonClick}
                    onDoubleClick={handleSendButtonDoubleClick}
                  >
                    <SendIcon className="h-5 w-5 text-[hsl(var(--primary))]" />
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
                      variant="secondary"
                      size="sm"
                      className={`font-mono text-xs ${key === 'ctrl+c' ? 'bg-red-700 text-white' : key === 'esc' || key === 'enter' ? 'bg-amber-700 text-white' : instant ? 'bg-[hsl(var(--primary))]/30 text-white' : ''}`}
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
                placeholder="Keys separated by whitespace. Press enter to send."
                value={customKeys}
                onChange={(e) => setCustomKeys(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === 'Enter') {
                    e.preventDefault()
                    handleSendCustomKeys(customKeys)
                  }
                }}
              />
            </div>
          </div>
          <div>
            <FileDropZone
              className="hidden sm:flex flex-col gap-2 rounded-md border border-dashed border-[hsl(var(--border))] bg-[hsl(var(--muted))]/20 px-3 py-3 transition-colors data-[disabled=true]:bg-[hsl(var(--muted))]/30 data-[disabled=true]:opacity-70 data-[drag-active=true]:border-[hsl(var(--primary))] data-[drag-active=true]:bg-[hsl(var(--primary))]/10"
              disabled={!uploadFile || isUploading}
              onFileDrop={handleUploadDrop}
            >
              <p className="text-center text-xs text-[hsl(var(--muted-foreground))]">
                Drop or paste a file here to upload on desktop.
              </p>
              <Button
                type="button"
                variant="secondary"
                size="sm"
                className="gap-2 w-full"
                disabled={!uploadFile || isUploading}
                onClick={handleUploadButtonClick}
              >
                <PaperclipIcon className="h-4 w-4" />
                {isUploading ? 'Uploading...' : 'Upload file'}
              </Button>
            </FileDropZone>
            <Button
              type="button"
              variant="secondary"
              size="sm"
              className="gap-2 w-full sm:hidden"
              disabled={!uploadFile || isUploading}
              onClick={handleUploadButtonClick}
            >
              <PaperclipIcon className="h-4 w-4" />
              {isUploading ? 'Uploading...' : 'Upload file'}
            </Button>
            <input
              ref={fileInputRef}
              type="file"
              className="hidden"
              onChange={handleFileInputChange}
            />
          </div>
        </div>
      </div>
      <div className="sm:hidden w-full h-10 flex justify-between items-center">
        <Button
          variant={'ghost'}
          className="text-[hsl(var(--primary))]"
          onClick={() => handleSendKeySpec('left')}
          aria-label="Left"
        >
          <ArrowLeftIcon className="w-6 h-6" />
        </Button>
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
          onClick={() => handleSendKeySpec('right')}
          aria-label="Right"
        >
          <ArrowRightIcon className="w-6 h-6" />
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
