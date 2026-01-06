import { Children, type ReactNode, memo, useCallback, useEffect, useMemo, useRef, useState } from "react"
import { invoke } from "@tauri-apps/api/core"
import { open as openDialog } from "@tauri-apps/plugin-dialog"
import { fetch as tauriFetch } from "@tauri-apps/plugin-http"
import { openPath } from "@tauri-apps/plugin-opener"
import type { PluggableList } from "unified"
import {
  AlertTriangle,
  Clock,
  Copy,
  Download,
  Power,
  PowerOff,
  Radio,
  RefreshCw,
  RotateCcw,
  Terminal as TerminalIcon,
  Trash2,
} from "lucide-react"
import { Streamdown, defaultRehypePlugins, defaultRemarkPlugins } from "streamdown"

const TOOL_LINE_PREFIXES = ["[tool_use]", "[tool_result]", "[thinking]"]
const ROOTS_STORAGE_KEY = "flow.desktop.roots"
const LIN_BASE_URL = "http://127.0.0.1:9050"
const MAX_SERVER_LOGS = 500
const SERVER_POLL_MS = 2000

const safeRehypePlugins = Object.entries(
  defaultRehypePlugins as Record<string, PluggableList[number]>,
)
  .filter(([key]) => key !== "raw")
  .map(([, plugin]) => plugin) as PluggableList

const streamdownProps = {
  mode: "static" as const,
  controls: false,
  rehypePlugins: safeRehypePlugins,
  remarkPlugins: Object.values(defaultRemarkPlugins),
  remarkRehypeOptions: {
    allowDangerousHtml: false,
  },
}

type DesktopProject = {
  name: string
  project_root: string
  config_path: string
  updated_ms: number
}

type WebSession = {
  id: string
  provider: string
  timestamp?: string | null
  name?: string | null
  messages: WebSessionMessage[]
  started_at?: string | null
  last_message_at?: string | null
}

type WebSessionMessage = {
  role: string
  content: string
}

type SessionWithProject = WebSession & {
  projectName?: string
  projectRoot?: string
}

type LogEntry = {
  project: string
  content: string
  timestamp: number
  log_type: string
  service: string
  stack?: string | null
  format: string
}

type StoredLogEntry = {
  id: number
  entry: LogEntry
}

type ServerSnapshot = {
  name: string
  command: string
  args: string[]
  port?: number
  working_dir?: string
  autostart: boolean
  status: string
  pid?: number
  exit_code?: number
  started_at?: number
  log_count?: number
}

type ServerLogEntry = {
  server: string
  timestamp_ms: number
  stream: "stdout" | "stderr" | string
  line: string
}

const normalizeRole = (role: string | undefined) => {
  const cleaned = role?.trim().toLowerCase() ?? "other"
  if (cleaned === "user") return "user"
  if (cleaned === "assistant") return "assistant"
  return "other"
}

const cleanMessageContent = (content: string) => {
  const lines = content.split(/\r?\n/)
  return lines
    .filter((line) => {
      const trimmed = line.trim()
      if (!trimmed) {
        return true
      }
      return !TOOL_LINE_PREFIXES.some((prefix) => trimmed.startsWith(prefix))
    })
    .join("\n")
    .trim()
}

const getDisplayMessages = (session: WebSession) => {
  const messages = session.messages ?? []
  return messages
    .map((message) => {
      const role = normalizeRole(message.role)
      if (role !== "user" && role !== "assistant") {
        return null
      }
      const content = cleanMessageContent(message.content ?? "")
      if (!content) {
        return null
      }
      return { role, content }
    })
    .filter((message): message is { role: string; content: string } => Boolean(message))
}

const truncateLabel = (value: string, max: number) => {
  const trimmed = value.trim()
  if (trimmed.length <= max) {
    return trimmed
  }
  return `${trimmed.slice(0, max).trim()}…`
}

const getSessionTitle = (session: WebSession) => {
  if (session.name && session.name.trim()) {
    return session.name.trim()
  }
  const messages = getDisplayMessages(session)
  const firstUser = messages.find((message) => message.role === "user")
  if (firstUser) {
    const line = firstUser.content.split(/\r?\n/).find((entry) => entry.trim())
    if (line) {
      return truncateLabel(line, 70)
    }
  }
  return "Session"
}

const getSessionPreview = (session: WebSession) => {
  const messages = getDisplayMessages(session)
  if (messages.length === 0) {
    return "No transcript available yet."
  }
  const lastMessage = messages[messages.length - 1]
  const lines = lastMessage.content.split(/\r?\n/)
  const preview = lines.slice(-4).join("\n").trim()
  return preview || lastMessage.content.slice(0, 160)
}

const parseTimestamp = (value?: string | null) => {
  if (!value) return null
  const parsed = Date.parse(value)
  return Number.isNaN(parsed) ? null : parsed
}

const formatRelativeTime = (timeMs: number | null) => {
  if (!timeMs) return "--"
  const delta = Date.now() - timeMs
  if (delta < 0) return "just now"

  const seconds = Math.floor(delta / 1000)
  const minutes = Math.floor(seconds / 60)
  const hours = Math.floor(minutes / 60)
  const days = Math.floor(hours / 24)

  const parts: string[] = []
  if (days) parts.push(`${days}d`)
  if (hours % 24) parts.push(`${hours % 24}h`)
  if (minutes % 60) parts.push(`${minutes % 60}m`)
  if (parts.length === 0) parts.push(`${Math.max(seconds % 60, 1)}s`)

  return `${parts.join(" ")} ago`
}

const extractText = (children: ReactNode) => {
  return Children.toArray(children)
    .map((child) => (typeof child === "string" ? child : ""))
    .join("")
}

const MarkdownCode = ({
  children,
  className,
}: {
  children?: ReactNode
  className?: string
}) => {
  const text = extractText(children)
  const isBlock = className?.includes("language-") || text.includes("\n")
  if (isBlock) {
    return <code className="plain-block mono">{text}</code>
  }
  return <code className="plain-inline mono">{text}</code>
}

const markdownComponents = {
  code: MarkdownCode,
}

const shortenPath = (value: string, max = 42) => {
  if (value.length <= max) return value
  const start = value.slice(0, Math.floor(max * 0.6))
  const end = value.slice(-Math.floor(max * 0.3))
  return `${start}…${end}`
}

const formatServerStatus = (status: string, exitCode?: number) => {
  const normalized = status.toLowerCase()
  if (normalized.includes("exit")) {
    if (exitCode === undefined || exitCode === null) return "stopped"
    if (exitCode === 0) return "stopped"
    return `failed (${exitCode})`
  }
  return status
}

const statusTone = (status: string, exitCode?: number) => {
  const normalized = status.toLowerCase()
  if (normalized.includes("running")) return "status-running"
  if (normalized.includes("starting")) return "status-starting"
  if (normalized.includes("exit")) {
    if (exitCode === 0) return "status-stopped"
    return "status-error"
  }
  if (normalized.includes("failed") || normalized.includes("error")) return "status-error"
  return "status-idle"
}

const formatUptime = (startedAt?: number) => {
  if (!startedAt) return "-"
  const ms = Date.now() - startedAt
  const seconds = Math.floor(ms / 1000)
  const minutes = Math.floor(seconds / 60)
  const hours = Math.floor(minutes / 60)
  const days = Math.floor(hours / 24)

  if (days > 0) return `${days}d ${hours % 24}h`
  if (hours > 0) return `${hours}h ${minutes % 60}m`
  if (minutes > 0) return `${minutes}m ${seconds % 60}s`
  return `${seconds}s`
}

const formatTime = (ms?: number | string) => {
  if (ms === undefined || ms === null) return ""
  const num = typeof ms === "string" ? Number(ms) : ms
  if (!Number.isFinite(num)) return ""
  try {
    return new Date(num).toLocaleTimeString()
  } catch {
    return ""
  }
}

const isRunning = (status: string) => status.toLowerCase().includes("running")

const getLastErrorOrExit = (logs: ServerLogEntry[]): string | null => {
  const recentLogs = logs.slice(-20).reverse()
  const errorPatterns = [
    /error[:\s]/i,
    /failed[:\s]/i,
    /exception[:\s]/i,
    /panic[:\s]/i,
    /EADDRINUSE/i,
    /port.*already in use/i,
    /cannot find/i,
    /not found/i,
    /permission denied/i,
    /EACCES/i,
    /ENOENT/i,
  ]

  for (const log of recentLogs) {
    if (log.stream === "stderr" && log.line.trim()) {
      for (const pattern of errorPatterns) {
        if (pattern.test(log.line)) {
          return log.line.trim()
        }
      }
    }
  }

  for (const log of recentLogs) {
    if (log.line.trim()) {
      for (const pattern of errorPatterns) {
        if (pattern.test(log.line)) {
          return log.line.trim()
        }
      }
    }
  }

  return null
}

const toLogEntries = (data: unknown, fallbackServer: string): ServerLogEntry[] => {
  const entries: ServerLogEntry[] = []
  const fallback = fallbackServer || "unknown"

  const coerceEntry = (item: unknown) => {
    if (item === null || item === undefined) return

    if (Array.isArray(item)) {
      for (const nested of item) coerceEntry(nested)
      return
    }

    if (typeof item === "string") {
      for (const line of item.split("\n")) {
        if (line === undefined) continue
        entries.push({
          server: fallback,
          timestamp_ms: Date.now(),
          stream: "stdout",
          line,
        })
      }
      return
    }

    if (typeof item === "object") {
      const obj = item as Record<string, unknown>
      const server = typeof obj.server === "string" ? obj.server : fallback
      const tsCandidate =
        typeof obj.timestamp_ms === "number"
          ? obj.timestamp_ms
          : typeof obj.timestamp_ms === "string"
            ? Number(obj.timestamp_ms)
            : typeof obj.timestamp === "number"
              ? obj.timestamp
              : typeof obj.timestamp === "string"
                ? Number(obj.timestamp)
                : Date.now()
      const ts = Number.isFinite(tsCandidate) ? (tsCandidate as number) : Date.now()
      const stream = typeof obj.stream === "string" ? obj.stream : "stdout"
      const line =
        typeof obj.line === "string"
          ? obj.line
          : obj.line === null || obj.line === undefined
            ? ""
            : JSON.stringify(obj.line ?? obj.message ?? obj.data ?? obj)
      entries.push({ server, timestamp_ms: ts, stream, line })
      return
    }

    entries.push({
      server: fallback,
      timestamp_ms: Date.now(),
      stream: "stdout",
      line: String(item),
    })
  }

  if (Array.isArray((data as Record<string, unknown>)?.logs)) {
    for (const item of (data as Record<string, unknown>).logs as unknown[]) coerceEntry(item)
    return entries
  }

  if (Array.isArray((data as Record<string, unknown>)?.lines)) {
    for (const item of (data as Record<string, unknown>).lines as unknown[]) coerceEntry(item)
    return entries
  }

  coerceEntry(data)
  return entries
}

const parseStreamPayload = (data: string, fallbackServer: string): ServerLogEntry[] => {
  try {
    return toLogEntries(JSON.parse(data), fallbackServer)
  } catch {
    return toLogEntries(data, fallbackServer)
  }
}

type CommandAction = {
  id: string
  label: string
  hint?: string
  run: () => void | Promise<void>
}

const CommandPalette = ({
  open,
  actions,
  onClose,
}: {
  open: boolean
  actions: CommandAction[]
  onClose: () => void
}) => {
  const [query, setQuery] = useState("")
  const [activeIndex, setActiveIndex] = useState(0)
  const inputRef = useRef<HTMLInputElement | null>(null)

  useEffect(() => {
    if (open) {
      setQuery("")
      setActiveIndex(0)
      requestAnimationFrame(() => inputRef.current?.focus())
    }
  }, [open])

  const filtered = useMemo(() => {
    if (!query.trim()) return actions
    const needle = query.toLowerCase()
    return actions.filter((action) => {
      const haystack = `${action.label} ${action.hint ?? ""}`.toLowerCase()
      return haystack.includes(needle)
    })
  }, [actions, query])

  const runAction = useCallback(
    (action?: CommandAction) => {
      if (!action) return
      Promise.resolve(action.run()).finally(() => {
        onClose()
      })
    },
    [onClose],
  )

  useEffect(() => {
    if (!open) return
    const handleKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault()
        onClose()
        return
      }
      if (event.key === "ArrowDown") {
        event.preventDefault()
        setActiveIndex((prev) => Math.min(prev + 1, filtered.length - 1))
        return
      }
      if (event.key === "ArrowUp") {
        event.preventDefault()
        setActiveIndex((prev) => Math.max(prev - 1, 0))
        return
      }
      if (event.key === "Enter") {
        event.preventDefault()
        runAction(filtered[activeIndex])
      }
    }
    window.addEventListener("keydown", handleKey)
    return () => window.removeEventListener("keydown", handleKey)
  }, [open, filtered, activeIndex, onClose, runAction])

  if (!open) return null

  return (
    <div className="command-overlay" onClick={onClose}>
      <div className="command-panel" onClick={(event) => event.stopPropagation()}>
        <input
          ref={inputRef}
          className="command-input"
          placeholder="Search commands"
          value={query}
          onChange={(event) => {
            setQuery(event.target.value)
            setActiveIndex(0)
          }}
        />
        <div className="command-list">
          {filtered.length === 0 ? (
            <div className="empty-state">No commands match that search.</div>
          ) : (
            filtered.map((action, index) => (
              <div
                key={action.id}
                className={`command-item ${index === activeIndex ? "active" : ""}`}
                onClick={() => runAction(action)}
              >
                <div className="command-title">{action.label}</div>
                {action.hint ? <div className="command-meta">{action.hint}</div> : null}
              </div>
            ))
          )}
        </div>
      </div>
    </div>
  )
}

const ServerTile = memo(({
  server,
  logs,
  baseUrl,
  onRefresh,
  onError,
  onFocus,
  onCopied,
  followLive,
  fetchWithCorsBypass,
}: {
  server: ServerSnapshot
  logs: ServerLogEntry[]
  baseUrl: string
  onRefresh: () => void
  onError: (message: string) => void
  onFocus?: () => void
  onCopied?: (label: string) => void
  followLive: boolean
  fetchWithCorsBypass: (url: string, init?: RequestInit) => Promise<Response>
}) => {
  const [controlling, setControlling] = useState(false)
  const logsEndRef = useRef<HTMLDivElement>(null)
  const running = isRunning(server.status)

  const lastError = useMemo(() => {
    if (running) return null
    return getLastErrorOrExit(logs)
  }, [running, logs])

  const startServer = async () => {
    setControlling(true)
    try {
      const res = await fetchWithCorsBypass(
        `${baseUrl}/servers/${encodeURIComponent(server.name)}/start`,
        { method: "POST" },
      )
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      onRefresh()
    } catch (error) {
      onError(
        `Failed to start ${server.name}: ${error instanceof Error ? error.message : String(error)}`,
      )
    } finally {
      setControlling(false)
    }
  }

  const stopServer = async () => {
    setControlling(true)
    try {
      const res = await fetchWithCorsBypass(
        `${baseUrl}/servers/${encodeURIComponent(server.name)}/stop`,
        { method: "POST" },
      )
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      onRefresh()
    } catch (error) {
      onError(
        `Failed to stop ${server.name}: ${error instanceof Error ? error.message : String(error)}`,
      )
    } finally {
      setControlling(false)
    }
  }

  const restartServer = async () => {
    setControlling(true)
    try {
      const res = await fetchWithCorsBypass(
        `${baseUrl}/servers/${encodeURIComponent(server.name)}/restart`,
        { method: "POST" },
      )
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      onRefresh()
    } catch (error) {
      onError(
        `Failed to restart ${server.name}: ${error instanceof Error ? error.message : String(error)}`,
      )
    } finally {
      setControlling(false)
    }
  }

  const clearLogs = async () => {
    try {
      const res = await fetchWithCorsBypass(
        `${baseUrl}/servers/${encodeURIComponent(server.name)}/logs/clear`,
        { method: "POST" },
      )
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      onRefresh()
    } catch (error) {
      onError(`Failed to clear logs: ${error instanceof Error ? error.message : String(error)}`)
    }
  }

  const copyLogs = () => {
    const text = logs
      .map(
        (line) =>
          `[${line.stream.toUpperCase()}] ${new Date(line.timestamp_ms).toISOString()} ${line.line}`,
      )
      .join("\n")
    navigator.clipboard.writeText(text)
    onCopied?.(server.name)
  }

  const exportLogs = () => {
    const text = logs
      .map(
        (line) =>
          `[${line.stream.toUpperCase()}] ${new Date(line.timestamp_ms).toISOString()} ${line.line}`,
      )
      .join("\n")
    const blob = new Blob([text], { type: "text/plain" })
    const url = URL.createObjectURL(blob)
    const anchor = document.createElement("a")
    anchor.href = url
    anchor.download = `${server.name}-${new Date().toISOString().slice(0, 19).replace(/:/g, "-")}.txt`
    document.body.appendChild(anchor)
    anchor.click()
    document.body.removeChild(anchor)
    URL.revokeObjectURL(url)
  }

  useEffect(() => {
    if (!followLive) return
    logsEndRef.current?.scrollIntoView({ behavior: "smooth" })
  }, [logs.length, followLive])

  return (
    <div className="server-tile" onDoubleClick={onFocus}>
      <div className="server-tile-header">
        <div className="server-header-left">
          <span className="server-name">{server.name}</span>
          <span className={`server-status-pill ${statusTone(server.status, server.exit_code)}`}>
            {formatServerStatus(server.status, server.exit_code)}
          </span>
        </div>
        <div className="server-controls">
          {running ? (
            <>
              <button
                className="icon-button"
                onClick={stopServer}
                disabled={controlling}
                title="Stop server"
              >
                {controlling ? <RefreshCw size={12} className="spin" /> : <PowerOff size={12} />}
              </button>
              <button
                className="icon-button"
                onClick={restartServer}
                disabled={controlling}
                title="Restart server"
              >
                <RotateCcw size={12} />
              </button>
            </>
          ) : (
            <button
              className="icon-button"
              onClick={startServer}
              disabled={controlling}
              title="Start server"
            >
              {controlling ? <RefreshCw size={12} className="spin" /> : <Power size={12} />}
            </button>
          )}
          <button className="icon-button" onClick={clearLogs} title="Clear logs">
            <Trash2 size={12} />
          </button>
          <button
            className="icon-button"
            onClick={copyLogs}
            disabled={logs.length === 0}
            title="Copy logs"
          >
            <Copy size={12} />
          </button>
          <button
            className="icon-button"
            onClick={exportLogs}
            disabled={logs.length === 0}
            title="Download logs"
          >
            <Download size={12} />
          </button>
        </div>
      </div>
      <div className="server-tile-info">
        <span className="info-pill">
          <TerminalIcon size={10} />
          {server.command} {server.args.join(" ")}
        </span>
        {server.port ? <span className="info-pill">:{server.port}</span> : null}
        {server.pid ? <span className="info-pill">pid {server.pid}</span> : null}
        {server.exit_code !== undefined && server.exit_code !== null ? (
          <span className="info-pill warning">exit {server.exit_code}</span>
        ) : null}
        {server.started_at ? (
          <span className="info-pill">
            <Clock size={9} />
            {formatUptime(server.started_at)}
          </span>
        ) : null}
      </div>
      {!running && lastError ? (
        <div className="server-error">
          <AlertTriangle size={12} />
          <span>{lastError}</span>
        </div>
      ) : null}
      <div className="server-tile-logs mono">
        {logs.length === 0 ? (
          <div className="server-log-empty">No logs yet</div>
        ) : (
          logs.map((log, index) => (
            <div key={`${log.timestamp_ms}-${index}`} className="server-log-row">
              <span className="server-log-time">{formatTime(log.timestamp_ms)}</span>
              <span className={`server-log-badge ${log.stream === "stderr" ? "err" : "out"}`}>
                {log.stream === "stderr" ? "err" : "out"}
              </span>
              <span className="server-log-message">{log.line}</span>
            </div>
          ))
        )}
        <div ref={logsEndRef} />
      </div>
    </div>
  )
})

const App = () => {
  const [projects, setProjects] = useState<DesktopProject[]>([])
  const [roots, setRoots] = useState<string[]>([])
  const [selectedProjectRoot, setSelectedProjectRoot] = useState<string | null>(null)
  const [sessions, setSessions] = useState<SessionWithProject[]>([])
  const [logs, setLogs] = useState<StoredLogEntry[]>([])
  const [view, setView] = useState<"sessions" | "logs" | "servers">("sessions")
  const [expandedSessions, setExpandedSessions] = useState<Set<string>>(new Set())
  const [paletteOpen, setPaletteOpen] = useState(false)
  const [loadingSessions, setLoadingSessions] = useState(false)
  const [loadingLogs, setLoadingLogs] = useState(false)
  const [servers, setServers] = useState<ServerSnapshot[]>([])
  const [serverLogs, setServerLogs] = useState<Record<string, ServerLogEntry[]>>({})
  const [loadingServers, setLoadingServers] = useState(false)
  const [serverError, setServerError] = useState<string | null>(null)
  const [daemonStatus, setDaemonStatus] = useState<"unknown" | "up" | "down">("unknown")
  const [lastServerUpdate, setLastServerUpdate] = useState<number | null>(null)
  const [followLive, setFollowLive] = useState(true)
  const [focusedServerIndex, setFocusedServerIndex] = useState<number | null>(null)
  const [copyToast, setCopyToast] = useState<string | null>(null)
  const streamRefs = useRef<Map<string, EventSource>>(new Map())
  const prevServerNamesRef = useRef<string>("")
  const pendingLogsRef = useRef<Map<string, ServerLogEntry[]>>(new Map())
  const flushTimerRef = useRef<number | null>(null)

  const selectedProject = useMemo(() => {
    return projects.find((project) => project.project_root === selectedProjectRoot) ?? null
  }, [projects, selectedProjectRoot])

  const normalizedLinBaseUrl = useMemo(() => LIN_BASE_URL.replace(/\/+$/, ""), [])
  const isTauri = useMemo(() => {
    if (typeof window === "undefined") return false
    const w = window as Window & { __TAURI_IPC__?: unknown; __TAURI__?: unknown }
    return Boolean(w.__TAURI_IPC__ || w.__TAURI__ || navigator.userAgent.includes("Tauri"))
  }, [])

  const fetchWithCorsBypass = useCallback(
    async (url: string, init?: RequestInit) => {
      if (isTauri) {
        return tauriFetch(url, init as Parameters<typeof tauriFetch>[1])
      }
      return fetch(url, init)
    },
    [isTauri],
  )

  const scheduleLogFlush = useCallback(() => {
    if (flushTimerRef.current !== null) return
    flushTimerRef.current = window.setTimeout(() => {
      setServerLogs((prev) => {
        const next = { ...prev }
        for (const [name, entries] of pendingLogsRef.current) {
          const existing = next[name] ?? []
          const merged = [...existing, ...entries].slice(-MAX_SERVER_LOGS)
          next[name] = merged
        }
        pendingLogsRef.current.clear()
        return next
      })
      flushTimerRef.current = null
    }, 200)
  }, [])

  useEffect(() => {
    const stored = localStorage.getItem(ROOTS_STORAGE_KEY)
    if (!stored) return
    try {
      const parsed = JSON.parse(stored)
      if (Array.isArray(parsed)) {
        setRoots(parsed.filter((entry) => typeof entry === "string"))
      }
    } catch {
      // Ignore corrupted storage
    }
  }, [])

  const persistRoots = useCallback((nextRoots: string[]) => {
    setRoots(nextRoots)
    localStorage.setItem(ROOTS_STORAGE_KEY, JSON.stringify(nextRoots))
  }, [])

  const refreshProjects = useCallback(async () => {
    try {
      const registered = (await invoke("list_projects")) as DesktopProject[]
      const discoveredLists = await Promise.all(
        roots.map((root) => invoke("discover_projects", { root }) as Promise<DesktopProject[]>),
      )
      const discovered = discoveredLists.flat()
      const map = new Map<string, DesktopProject>()
      for (const project of [...registered, ...discovered]) {
        map.set(project.config_path || project.project_root, project)
      }
      const merged = Array.from(map.values()).sort((a, b) => a.name.localeCompare(b.name))
      setProjects(merged)

      if (
        selectedProjectRoot &&
        !merged.some((project) => project.project_root === selectedProjectRoot)
      ) {
        setSelectedProjectRoot(null)
      }
    } catch (error) {
      console.error("Failed to refresh projects", error)
      setProjects([])
      setSelectedProjectRoot(null)
    }
  }, [roots, selectedProjectRoot])

  useEffect(() => {
    refreshProjects()
  }, [refreshProjects])

  const addRoot = useCallback(async () => {
    const selection = await openDialog({ directory: true, multiple: false })
    if (!selection || typeof selection !== "string") {
      return
    }
    if (roots.includes(selection)) {
      return
    }
    const nextRoots = [...roots, selection]
    persistRoots(nextRoots)
  }, [roots, persistRoots])

  const loadSessions = useCallback(async () => {
    setLoadingSessions(true)
    try {
      if (projects.length === 0) {
        setSessions([])
        return
      }
      if (selectedProjectRoot) {
        const data = (await invoke("sessions_for_project", {
          projectRoot: selectedProjectRoot,
        })) as WebSession[]
        setSessions(
          data.map((session) => ({
            ...session,
            projectName: selectedProject?.name,
            projectRoot: selectedProjectRoot,
          })),
        )
        return
      }
      const all = await Promise.all(
        projects.map(async (project) => {
          const projectSessions = (await invoke("sessions_for_project", {
            projectRoot: project.project_root,
          })) as WebSession[]
          return projectSessions.map((session) => ({
            ...session,
            projectName: project.name,
            projectRoot: project.project_root,
          }))
        }),
      )
      setSessions(all.flat())
    } catch (error) {
      console.error("Failed to load sessions", error)
      setSessions([])
    } finally {
      setLoadingSessions(false)
    }
  }, [projects, selectedProjectRoot, selectedProject])

  const loadLogs = useCallback(async () => {
    setLoadingLogs(true)
    try {
      const projectName = selectedProject?.name && selectedProjectRoot ? selectedProject.name : null
      const data = (await invoke("logs_for_project", {
        project: projectName,
        limit: 200,
      })) as StoredLogEntry[]
      setLogs(data)
    } catch (error) {
      console.error("Failed to load logs", error)
      setLogs([])
    } finally {
      setLoadingLogs(false)
    }
  }, [selectedProject, selectedProjectRoot])

  const fetchServers = useCallback(async () => {
    setLoadingServers(true)
    setServerError(null)
    try {
      const res = await fetchWithCorsBypass(`${normalizedLinBaseUrl}/servers`)
      if (!res.ok) {
        throw new Error(`HTTP ${res.status}`)
      }
      const data = (await res.json()) as ServerSnapshot[]
      setServers(data)
      setDaemonStatus("up")
      setLastServerUpdate(Date.now())
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error)
      setServerError(`Failed to load servers: ${msg}`)
      setDaemonStatus("down")
      setServers([])
    } finally {
      setLoadingServers(false)
    }
  }, [fetchWithCorsBypass, normalizedLinBaseUrl])

  const fetchServerLogs = useCallback(
    async (serverName: string) => {
      const path = `${normalizedLinBaseUrl}/servers/${encodeURIComponent(serverName)}/logs?limit=${MAX_SERVER_LOGS}`
      try {
        const res = await fetchWithCorsBypass(path)
        if (!res.ok) return
        const data = await res.json()
        const entries = toLogEntries(data, serverName)
        setServerLogs((prev) => ({
          ...prev,
          [serverName]: entries.slice(-MAX_SERVER_LOGS),
        }))
      } catch {
        // Ignore log fetch errors per server.
      }
    },
    [fetchWithCorsBypass, normalizedLinBaseUrl],
  )

  const fetchAllServerLogs = useCallback(
    async (serverList: ServerSnapshot[]) => {
      for (const server of serverList) {
        await fetchServerLogs(server.name)
      }
    },
    [fetchServerLogs],
  )

  useEffect(() => {
    if (view !== "sessions") return
    loadSessions()
  }, [view, loadSessions])

  useEffect(() => {
    if (view !== "logs") return
    loadLogs()
    const interval = window.setInterval(loadLogs, 5000)
    return () => window.clearInterval(interval)
  }, [view, loadLogs])

  useEffect(() => {
    if (view !== "servers") return
    fetchServers()
    const interval = window.setInterval(fetchServers, SERVER_POLL_MS)
    return () => window.clearInterval(interval)
  }, [view, fetchServers])

  useEffect(() => {
    if (view !== "servers") return
    if (servers.length === 0) return
    const currentNames = servers.map((server) => server.name).join(",")
    if (!currentNames || currentNames === prevServerNamesRef.current) return
    prevServerNamesRef.current = currentNames
    fetchAllServerLogs(servers)
  }, [view, servers, fetchAllServerLogs])

  useEffect(() => {
    if (view !== "servers") {
      for (const source of streamRefs.current.values()) {
        source.close()
      }
      streamRefs.current.clear()
      pendingLogsRef.current.clear()
      if (flushTimerRef.current !== null) {
        window.clearTimeout(flushTimerRef.current)
        flushTimerRef.current = null
      }
      return
    }
    if (!followLive) {
      for (const source of streamRefs.current.values()) {
        source.close()
      }
      streamRefs.current.clear()
      pendingLogsRef.current.clear()
      if (flushTimerRef.current !== null) {
        window.clearTimeout(flushTimerRef.current)
        flushTimerRef.current = null
      }
      return
    }

    const names = servers.map((server) => server.name)
    for (const [name, source] of streamRefs.current) {
      if (!names.includes(name)) {
        source.close()
        streamRefs.current.delete(name)
      }
    }

    for (const name of names) {
      if (streamRefs.current.has(name)) continue
      const streamPath = `${normalizedLinBaseUrl}/servers/${encodeURIComponent(name)}/logs/stream`
      const source = new EventSource(streamPath)
      streamRefs.current.set(name, source)

      source.onmessage = (event) => {
        const entries = parseStreamPayload(event.data, name)
        if (entries.length === 0) return
        const pending = pendingLogsRef.current.get(name) ?? []
        pendingLogsRef.current.set(name, [...pending, ...entries])
        scheduleLogFlush()
      }

      source.onerror = () => {
        source.close()
        streamRefs.current.delete(name)
      }
    }

    return () => {
      for (const source of streamRefs.current.values()) {
        source.close()
      }
      streamRefs.current.clear()
    }
  }, [view, followLive, servers, normalizedLinBaseUrl, scheduleLogFlush])

  useEffect(() => {
    const handleKey = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault()
        setPaletteOpen(true)
      }
    }
    window.addEventListener("keydown", handleKey)
    return () => window.removeEventListener("keydown", handleKey)
  }, [])

  useEffect(() => {
    if (view !== "servers") return
    const handleEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setFocusedServerIndex(null)
      }
    }
    window.addEventListener("keydown", handleEscape)
    return () => window.removeEventListener("keydown", handleEscape)
  }, [view])

  const toggleSession = (sessionKey: string) => {
    setExpandedSessions((prev) => {
      const next = new Set(prev)
      if (next.has(sessionKey)) {
        next.delete(sessionKey)
      } else {
        next.add(sessionKey)
      }
      return next
    })
  }

  const commandActions = useMemo<CommandAction[]>(() => {
    const actions: CommandAction[] = [
      {
        id: "add-root",
        label: "Add workspace root",
        hint: "Scan for flow.toml",
        run: addRoot,
      },
      {
        id: "refresh",
        label: "Refresh projects",
        hint: "Reload sessions and logs",
        run: refreshProjects,
      },
      {
        id: "view-sessions",
        label: "Show sessions",
        run: () => setView("sessions"),
      },
      {
        id: "view-logs",
        label: "Show logs",
        run: () => setView("logs"),
      },
      {
        id: "view-servers",
        label: "Show servers",
        run: () => setView("servers"),
      },
      {
        id: "view-all",
        label: "View all projects",
        hint: "Aggregate sessions and logs",
        run: () => setSelectedProjectRoot(null),
      },
    ]

    if (selectedProject) {
      actions.push({
        id: "open-folder",
        label: "Open project folder",
        hint: shortenPath(selectedProject.project_root),
        run: () => openPath(selectedProject.project_root),
      })
    }

    projects.forEach((project) => {
      actions.push({
        id: `project:${project.project_root}`,
        label: `Switch to ${project.name}`,
        hint: shortenPath(project.project_root),
        run: () => setSelectedProjectRoot(project.project_root),
      })
    })

    return actions
  }, [addRoot, refreshProjects, selectedProject, projects])

  const headerTitle =
    view === "servers" ? "Servers" : selectedProject?.name ?? "All projects"
  const headerMeta =
    view === "servers"
      ? `Lin daemon: ${normalizedLinBaseUrl}`
      : selectedProject
        ? shortenPath(selectedProject.project_root)
        : "Watching every flow.toml you added"

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">
          <h1>flow</h1>
        </div>

        <div className="sidebar-actions">
          <button className="button" onClick={() => setPaletteOpen(true)}>
            Command palette
          </button>
          <button className="button" onClick={addRoot}>
            Add workspace root
          </button>
        </div>

        <div className="sidebar-section">
          <h2>Projects</h2>
          <div className="project-list">
            <div
              className={`project-card ${selectedProjectRoot === null ? "active" : ""}`}
              onClick={() => setSelectedProjectRoot(null)}
            >
              <div className="project-title">All projects</div>
              <div className="project-path">{projects.length} tracked</div>
            </div>
            {projects.map((project) => (
              <div
                key={project.config_path || project.project_root}
                className={`project-card ${
                  selectedProjectRoot === project.project_root ? "active" : ""
                }`}
                onClick={() => setSelectedProjectRoot(project.project_root)}
              >
                <div className="project-title">{project.name}</div>
                <div className="project-path">{shortenPath(project.project_root)}</div>
              </div>
            ))}
          </div>
        </div>
      </aside>

      <main className="main">
        <header className="header">
          <div>
            <h2>{headerTitle}</h2>
            <div className="meta">{headerMeta}</div>
          </div>
          <div className="tabs">
            <button
              className={`tab ${view === "sessions" ? "active" : ""}`}
              onClick={() => setView("sessions")}
            >
              Sessions
            </button>
            <button
              className={`tab ${view === "logs" ? "active" : ""}`}
              onClick={() => setView("logs")}
            >
              Logs
            </button>
            <button
              className={`tab ${view === "servers" ? "active" : ""}`}
              onClick={() => setView("servers")}
            >
              Servers
            </button>
          </div>
        </header>

        <section className="content">
          {view === "sessions" ? (
            <div className="section">
              {loadingSessions ? <div className="empty-state">Loading sessions…</div> : null}
              {!loadingSessions && sessions.length === 0 ? (
                <div className="empty-state">
                  No sessions yet. Run a Flow task or Claude/Codex session to populate history.
                </div>
              ) : null}
              {sessions.map((session) => {
                const sessionKey = `${session.projectRoot ?? "all"}:${session.id}`
                const expanded = expandedSessions.has(sessionKey)
                const started = parseTimestamp(session.started_at)
                const last = parseTimestamp(session.last_message_at)
                const preview = getSessionPreview(session)
                return (
                  <div
                    key={sessionKey}
                    className="session-card"
                    onClick={() => toggleSession(sessionKey)}
                  >
                    <div className="session-header">
                      <div>
                        <div className="session-provider">{session.provider}</div>
                        <div className="session-title">{getSessionTitle(session)}</div>
                        {selectedProjectRoot ? null : session.projectName ? (
                          <div className="meta">{session.projectName}</div>
                        ) : null}
                      </div>
                      <div className="session-times">
                        <div>Start {formatRelativeTime(started)}</div>
                        <div>Last {formatRelativeTime(last ?? started)}</div>
                      </div>
                    </div>

                    {!expanded ? (
                      <div className="session-preview">
                        <Streamdown {...streamdownProps} components={markdownComponents}>
                          {preview}
                        </Streamdown>
                      </div>
                    ) : (
                      <div className="session-messages">
                        {getDisplayMessages(session).map((message, index) => (
                          <div
                            key={`${sessionKey}-${index}`}
                            className={`message ${message.role}`}
                            onClick={(event) => event.stopPropagation()}
                          >
                            <div className="message-role">{message.role}</div>
                            <div className="message-content">
                              <Streamdown {...streamdownProps} components={markdownComponents}>
                                {message.content}
                              </Streamdown>
                            </div>
                          </div>
                        ))}
                      </div>
                    )}
                  </div>
                )
              })}
            </div>
          ) : view === "logs" ? (
            <div className="logs-panel">
              {loadingLogs ? <div className="empty-state">Loading logs…</div> : null}
              {!loadingLogs && logs.length === 0 ? (
                <div className="empty-state">No logs yet for this selection.</div>
              ) : null}
              {logs.map((log) => (
                <div key={log.id} className="log-row">
                  <div className="log-time">
                    {new Date(log.entry.timestamp).toLocaleTimeString()}
                  </div>
                  <div className="log-service">{log.entry.service}</div>
                  <div className="log-content">
                    {selectedProjectRoot ? null : (
                      <div className="meta">{log.entry.project}</div>
                    )}
                    {log.entry.content}
                  </div>
                </div>
              ))}
            </div>
          ) : (
            <div className="servers-view">
              <div className="servers-topbar">
                <div className="servers-status">
                  <span className={`daemon-pill ${daemonStatus}`}>
                    <Radio size={10} />
                    {daemonStatus === "up"
                      ? "daemon running"
                      : daemonStatus === "down"
                        ? "daemon down"
                        : "daemon"}
                  </span>
                  {focusedServerIndex !== null && servers[focusedServerIndex] ? (
                    <button
                      className="focused-pill"
                      onClick={() => setFocusedServerIndex(null)}
                    >
                      Focused: {servers[focusedServerIndex].name}
                      <span>(Esc)</span>
                    </button>
                  ) : null}
                  {lastServerUpdate ? (
                    <span className="servers-updated">
                      Updated {new Date(lastServerUpdate).toLocaleTimeString()}
                    </span>
                  ) : null}
                </div>
                <div className="servers-actions">
                  <button className="ghost-button" onClick={fetchServers} disabled={loadingServers}>
                    <RefreshCw size={12} className={loadingServers ? "spin" : ""} />
                    <span>Refresh</span>
                  </button>
                  <button
                    className={`ghost-button ${followLive ? "active" : ""}`}
                    onClick={() => setFollowLive(!followLive)}
                  >
                    <span>{followLive ? "Live" : "Paused"}</span>
                  </button>
                </div>
              </div>

              {serverError ? (
                <div className="server-error-banner">
                  <AlertTriangle size={12} />
                  <div>{serverError}</div>
                </div>
              ) : null}

              <div className="servers-grid">
                {servers.length === 0 ? (
                  <div className="empty-state">
                    <div className="empty-title">No servers configured</div>
                    <div className="empty-subtitle">
                      Make sure the lin daemon is running: <code>lin daemon</code>
                    </div>
                  </div>
                ) : focusedServerIndex !== null && servers[focusedServerIndex] ? (
                  <ServerTile
                    key={servers[focusedServerIndex].name}
                    server={servers[focusedServerIndex]}
                    logs={serverLogs[servers[focusedServerIndex].name] || []}
                    baseUrl={normalizedLinBaseUrl}
                    followLive={followLive}
                    onRefresh={() => {
                      fetchServers()
                      fetchServerLogs(servers[focusedServerIndex].name)
                    }}
                    onError={setServerError}
                    onCopied={(label) => {
                      setCopyToast(label)
                      setTimeout(() => setCopyToast(null), 2000)
                    }}
                    onFocus={() => setFocusedServerIndex(null)}
                    fetchWithCorsBypass={fetchWithCorsBypass}
                  />
                ) : (
                  <div className={`server-tile-grid ${servers.length === 1 ? "single" : ""}`}>
                    {servers.map((server, index) => (
                      <ServerTile
                        key={server.name}
                        server={server}
                        logs={serverLogs[server.name] || []}
                        baseUrl={normalizedLinBaseUrl}
                        followLive={followLive}
                        onRefresh={() => {
                          fetchServers()
                          fetchServerLogs(server.name)
                        }}
                        onError={setServerError}
                        onCopied={(label) => {
                          setCopyToast(label)
                          setTimeout(() => setCopyToast(null), 2000)
                        }}
                        onFocus={() => setFocusedServerIndex(index)}
                        fetchWithCorsBypass={fetchWithCorsBypass}
                      />
                    ))}
                  </div>
                )}
              </div>

              {copyToast ? (
                <div className="copy-toast">
                  <Copy size={12} />
                  Copied output of {copyToast}
                </div>
              ) : null}
            </div>
          )}
        </section>
      </main>

      <CommandPalette open={paletteOpen} actions={commandActions} onClose={() => setPaletteOpen(false)} />
    </div>
  )
}

export default App
