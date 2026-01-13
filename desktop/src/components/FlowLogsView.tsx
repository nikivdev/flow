import { homeDir, join } from "@tauri-apps/api/path"
import { exists, readFile } from "@tauri-apps/plugin-fs"
import { fetch as tauriFetch } from "@tauri-apps/plugin-http"
import {
  Activity,
  AlertTriangle,
  Pause,
  Play,
  Radio,
  RefreshCw,
  ScrollText,
  TimerReset,
} from "lucide-react"
import { useCallback, useEffect, useMemo, useRef, useState } from "react"
import { useTTYStreams } from "../hooks/use-tty-streams"

const DEFAULT_HOST = "127.0.0.1"
const DEFAULT_PORT = 9050
const DEFAULT_LIMIT = 200
const MAX_LOGS = 2000
const INITIAL_BACKOFF_MS = 1200
const MAX_BACKOFF_MS = 12000
const MAX_HISTORY_ITEMS = 40

type ServerOption = {
  value: string
  label: string
}

type LogEntry = {
  server: string
  stream: string
  timestamp_ms: number
  line: unknown
}

const formatTime = (ms?: number) => {
  if (ms === undefined || ms === null) return ""
  const date = new Date(ms)
  if (Number.isNaN(date.getTime())) return ""
  return date.toLocaleTimeString()
}

const stringifyLine = (line: unknown) => {
  if (typeof line === "string") return line
  if (line === null || line === undefined) return ""
  if (typeof line === "object") {
    try {
      return JSON.stringify(line)
    } catch {
      return String(line)
    }
  }
  return String(line)
}

const normalizeLogEntry = (entry: unknown, fallbackServer: string): LogEntry | null => {
  if (entry === undefined || entry === null) return null
  if (typeof entry === "string") {
    return {
      server: fallbackServer,
      stream: "stdout",
      timestamp_ms: Date.now(),
      line: entry,
    }
  }
  if (typeof entry !== "object") return null
  const raw = entry as Record<string, unknown>

  const rawTimestamp = raw.timestamp_ms ?? raw.timestamp
  const parsedTimestamp =
    typeof rawTimestamp === "string" || typeof rawTimestamp === "number"
      ? Number(rawTimestamp)
      : Date.now()

  const server =
    typeof raw.server === "string"
      ? raw.server
      : typeof raw.name === "string"
        ? raw.name
        : fallbackServer

  const stream =
    typeof raw.stream === "string"
      ? raw.stream
      : typeof raw.channel === "string"
        ? raw.channel
        : "stdout"

  const line = raw.line ?? raw.message ?? raw.data ?? raw

  return {
    server,
    stream,
    timestamp_ms: Number.isFinite(parsedTimestamp) ? parsedTimestamp : Date.now(),
    line,
  }
}

const CommandList = ({
  commands,
  loading,
  error,
  onRefresh,
  logDir,
  metaDir,
}: {
  commands: ReturnType<typeof useTTYStreams>["commands"]
  loading: boolean
  error?: string | null
  onRefresh: () => void
  logDir?: string
  metaDir?: string
}) => {
  const items = commands.slice(0, 6)

  return (
    <div className="flow-logs-card">
      <div className="flow-logs-card-header">
        <div className="flow-logs-card-title">
          <Activity size={12} />
          Flow commands
        </div>
        <div className="flow-logs-card-actions">
          {logDir ? <span className="flow-logs-chip">logs: {logDir}</span> : null}
          {metaDir ? <span className="flow-logs-chip">meta: {metaDir}</span> : null}
          <button className="ghost-button small" onClick={onRefresh} disabled={loading}>
            <RefreshCw size={12} className={loading ? "spin" : ""} />
            <span>Refresh</span>
          </button>
        </div>
      </div>
      {error ? <div className="flow-logs-warning">{error}</div> : null}
      {items.length === 0 ? (
        <div className="flow-logs-empty">
          {loading ? "Connecting to flow trace…" : "No commands recorded yet."}
        </div>
      ) : (
        <div className="flow-logs-list">
          {items.map((cmd) => {
            const status =
              cmd.endedAt === undefined
                ? "running"
                : (cmd.status ?? 0) === 0
                  ? "success"
                  : "error"

            const preview =
              cmd.output.find((line) => line.trim().length > 0)?.slice(0, 200) ??
              cmd.command ??
              "(no output yet)"

            return (
              <div key={cmd.id} className={`flow-logs-command ${status}`}>
                <div className="flow-logs-command-row">
                  <div>
                    <div className="flow-logs-command-title">
                      {cmd.command ?? "(unknown command)"}
                    </div>
                    <div className="flow-logs-command-meta">{cmd.cwd ?? "(cwd unknown)"}</div>
                  </div>
                  <span className={`flow-logs-pill ${status}`}>
                    {status === "running" ? "Running" : status === "success" ? "Exit 0" : "Error"}
                  </span>
                </div>
                <div className="flow-logs-command-preview">{preview}</div>
              </div>
            )
          })}
        </div>
      )}
    </div>
  )
}

const FlowHistoryCard = ({
  entries,
  loading,
  error,
  onReload,
  filePath,
}: {
  entries: {
    command: string
    project: string
    status?: number | null
    success: boolean
    timestamp: number
  }[]
  loading: boolean
  error: string | null
  onReload: () => void
  filePath?: string
}) => {
  return (
    <div className="flow-logs-card">
      <div className="flow-logs-card-header">
        <div className="flow-logs-card-title">
          <Activity size={12} />
          Flow history
        </div>
        <button className="ghost-button small" onClick={onReload} disabled={loading}>
          <RefreshCw size={12} className={loading ? "spin" : ""} />
          <span>Reload</span>
        </button>
      </div>
      {filePath ? <div className="flow-logs-chip">History: {filePath}</div> : null}
      {error ? <div className="flow-logs-warning">{error}</div> : null}
      {entries.length === 0 ? (
        <div className="flow-logs-empty">{loading ? "Loading history…" : "No history entries."}</div>
      ) : (
        <div className="flow-logs-list">
          {entries.slice(0, 8).map((entry, idx) => (
            <div key={`${entry.command}-${entry.timestamp}-${idx}`} className="flow-logs-history">
              <div className="flow-logs-command-title">{entry.command}</div>
              {entry.project ? (
                <div className="flow-logs-command-meta">{entry.project}</div>
              ) : null}
              <div className="flow-logs-history-meta">
                <span>{new Date(entry.timestamp).toLocaleTimeString()}</span>
                <span className={`flow-logs-pill ${entry.success ? "success" : "error"}`}>
                  {entry.success ? "Exit 0" : `Exit ${entry.status ?? "?"}`}
                </span>
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

const FlowLogsView = () => {
  const [host, setHost] = useState(DEFAULT_HOST)
  const [port, setPort] = useState(DEFAULT_PORT.toString())
  const [prefillLimit, setPrefillLimit] = useState(DEFAULT_LIMIT)
  const [serverOptions, setServerOptions] = useState<ServerOption[]>([
    { value: "all", label: "All servers" },
  ])
  const [selectedServer, setSelectedServer] = useState("all")
  const [logs, setLogs] = useState<LogEntry[]>([])
  const [isStreaming, setIsStreaming] = useState(true)
  const [connecting, setConnecting] = useState(false)
  const [connected, setConnected] = useState(false)
  const [historyLoading, setHistoryLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [lastEvent, setLastEvent] = useState<number | null>(null)
  const [loadingServers, setLoadingServers] = useState(false)
  const [streamSession, setStreamSession] = useState(0)
  const [autoScroll, setAutoScroll] = useState(true)
  const [historyEntries, setHistoryEntries] = useState<
    {
      command: string
      project: string
      status?: number | null
      success: boolean
      timestamp: number
    }[]
  >([])
  const [historyError, setHistoryError] = useState<string | null>(null)
  const [historyFilePath, setHistoryFilePath] = useState<string>()
  const [historyFileLoading, setHistoryFileLoading] = useState(false)

  const streamRef = useRef<EventSource | null>(null)
  const reconnectTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const backoffRef = useRef(INITIAL_BACKOFF_MS)
  const logContainerRef = useRef<HTMLDivElement | null>(null)

  const {
    commands,
    loading: commandsLoading,
    error: commandsError,
    logDir,
    metaDir,
    refresh,
  } = useTTYStreams({
    baseDir: ".flow",
    logDirName: "tmux-logs",
    metaDirName: "tty-meta",
    label: "flow",
  })

  const normalizedBaseUrl = useMemo(() => {
    const sanitizedHost = host.trim().replace(/^https?:\/\//, "") || DEFAULT_HOST
    const safePort = Number(port) > 0 ? Number(port) : DEFAULT_PORT
    return `http://${sanitizedHost}:${safePort}`
  }, [host, port])

  const streamPath = useMemo(
    () =>
      selectedServer === "all"
        ? "/logs/stream"
        : `/servers/${encodeURIComponent(selectedServer)}/logs/stream`,
    [selectedServer],
  )

  const historyPath = useMemo(
    () =>
      selectedServer === "all"
        ? `/logs?limit=${prefillLimit}`
        : `/servers/${encodeURIComponent(selectedServer)}/logs?limit=${prefillLimit}`,
    [prefillLimit, selectedServer],
  )

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

  const fetchServers = useCallback(async () => {
    setLoadingServers(true)
    try {
      const res = await fetchWithCorsBypass(`${normalizedBaseUrl}/servers`)
      if (!res.ok) {
        throw new Error(`HTTP ${res.status}`)
      }
      const body = await res.json()
      const names = (Array.isArray(body) ? body : [])
        .map((srv: any, idx: number) => {
          if (srv && typeof srv === "object") {
            if (typeof srv.name === "string") return srv.name
            if (typeof srv.id === "string") return srv.id
          }
          return `server-${idx + 1}`
        })
        .filter(Boolean)
      const uniqueNames = Array.from(new Set(names))
      setServerOptions([
        { value: "all", label: "All servers" },
        ...uniqueNames.map((name) => ({ value: name, label: name })),
      ])
      if (selectedServer !== "all" && !uniqueNames.includes(selectedServer)) {
        setSelectedServer("all")
      }
    } catch (err) {
      console.error("Failed to load flow servers", err)
    } finally {
      setLoadingServers(false)
    }
  }, [fetchWithCorsBypass, normalizedBaseUrl, selectedServer])

  const fetchHistory = useCallback(async (): Promise<boolean> => {
    setHistoryLoading(true)
    setError(null)
    try {
      const res = await fetchWithCorsBypass(`${normalizedBaseUrl}${historyPath}`)
      if (!res.ok) {
        throw new Error(`HTTP ${res.status}`)
      }
      const body = await res.json()
      const normalized = (Array.isArray(body) ? body : [])
        .map((entry) => normalizeLogEntry(entry, selectedServer))
        .filter(Boolean) as LogEntry[]
      setLogs(normalized.slice(-MAX_LOGS))
      setLastEvent(Date.now())
      return true
    } catch (err) {
      console.error("Failed to load flow logs", err)
      const msg = err instanceof Error ? err.message : String(err)
      setError(`Unable to load logs from ${normalizedBaseUrl}. ${msg}`)
      return false
    } finally {
      setHistoryLoading(false)
    }
  }, [fetchWithCorsBypass, historyPath, normalizedBaseUrl, selectedServer])

  const parseSseData = useCallback(
    (payload: string): LogEntry | null => {
      try {
        const parsed = JSON.parse(payload)
        const normalized = normalizeLogEntry(parsed, selectedServer)
        if (normalized) return normalized
      } catch {
        // Fall back to string payload
      }
      return {
        server: selectedServer === "all" ? "flow" : selectedServer,
        stream: "stdout",
        timestamp_ms: Date.now(),
        line: payload,
      }
    },
    [selectedServer],
  )

  const closeStream = useCallback(() => {
    if (streamRef.current) {
      streamRef.current.close()
      streamRef.current = null
    }
    setConnecting(false)
    setConnected(false)
  }, [])

  useEffect(() => {
    return () => {
      if (reconnectTimerRef.current) {
        clearTimeout(reconnectTimerRef.current)
        reconnectTimerRef.current = null
      }
      closeStream()
    }
  }, [closeStream])

  useEffect(() => {
    if (!isStreaming) {
      closeStream()
      return
    }

    if (typeof EventSource === "undefined") {
      setError("EventSource is not available in this environment.")
      setIsStreaming(false)
      return
    }

    let cancelled = false
    backoffRef.current = INITIAL_BACKOFF_MS

    const connect = async (withPrefill: boolean) => {
      if (cancelled) return

      setConnecting(true)
      if (withPrefill) {
        await fetchHistory()
      }

      if (cancelled || !isStreaming) return

      try {
        const source = new EventSource(`${normalizedBaseUrl}${streamPath}`)
        streamRef.current = source

        source.onopen = () => {
          if (cancelled) return
          setConnected(true)
          setConnecting(false)
          setError(null)
          backoffRef.current = INITIAL_BACKOFF_MS
        }

        source.onmessage = (event) => {
          if (cancelled) return
          const entry = parseSseData(event.data)
          if (!entry) return
          setLogs((prev) => {
            const next = [...prev, entry]
            return next.slice(-MAX_LOGS)
          })
          setLastEvent(Date.now())
        }

        source.onerror = () => {
          if (cancelled) return
          setConnected(false)
          setConnecting(false)
          setError("Stream dropped. Reconnecting...")
          source.close()
          streamRef.current = null
          const delay = backoffRef.current
          reconnectTimerRef.current = setTimeout(() => connect(false), delay)
          backoffRef.current = Math.min(Math.round(backoffRef.current * 1.5), MAX_BACKOFF_MS)
        }
      } catch (err) {
        if (cancelled) return
        console.error("Failed to open flow stream", err)
        setConnected(false)
        setConnecting(false)
        setError("Unable to open SSE stream. Check the daemon and URL.")
        const delay = backoffRef.current
        reconnectTimerRef.current = setTimeout(() => connect(false), delay)
        backoffRef.current = Math.min(Math.round(backoffRef.current * 1.5), MAX_BACKOFF_MS)
      }
    }

    connect(true)

    return () => {
      cancelled = true
      if (reconnectTimerRef.current) {
        clearTimeout(reconnectTimerRef.current)
        reconnectTimerRef.current = null
      }
      closeStream()
    }
  }, [
    closeStream,
    fetchHistory,
    isStreaming,
    normalizedBaseUrl,
    parseSseData,
    streamPath,
    streamSession,
  ])

  useEffect(() => {
    if (autoScroll && logContainerRef.current) {
      logContainerRef.current.scrollTop = logContainerRef.current.scrollHeight
    }
  }, [logs, autoScroll])

  useEffect(() => {
    fetchServers()
  }, [fetchServers])

  const loadFlowHistory = useCallback(async () => {
    setHistoryFileLoading(true)
    setHistoryError(null)
    try {
      const home = await homeDir()
      const path = await join(home, ".config", "flow", "history.jsonl")
      setHistoryFilePath(path)
      const fileExists = await exists(path)
      if (!fileExists) {
        setHistoryEntries([])
        setHistoryError("No history file yet. Run a flow task to record history.")
        return
      }
      const contents = await readFile(path)
      const text = new TextDecoder().decode(contents)
      const lines = text
        .split("\n")
        .map((l) => l.trim())
        .filter(Boolean)
        .reverse()

      const entries = []
      for (const line of lines) {
        if (entries.length >= MAX_HISTORY_ITEMS) break
        try {
          const parsed = JSON.parse(line) as any
          entries.push({
            command: parsed.user_input || parsed.task_name || parsed.command || "(unknown)",
            project: parsed.project_root || "",
            status: parsed.status,
            success: Boolean(parsed.success),
            timestamp: Number(parsed.timestamp_ms) || Date.now(),
          })
        } catch (err) {
          console.warn("Failed to parse flow history line", err)
        }
      }
      setHistoryEntries(entries)
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err)
      setHistoryError(message)
    } finally {
      setHistoryFileLoading(false)
    }
  }, [])

  useEffect(() => {
    loadFlowHistory()
  }, [loadFlowHistory])

  const handleReconnect = useCallback(() => {
    backoffRef.current = INITIAL_BACKOFF_MS
    if (reconnectTimerRef.current) {
      clearTimeout(reconnectTimerRef.current)
      reconnectTimerRef.current = null
    }
    closeStream()
    setError(null)
    setStreamSession((session) => session + 1)
    setIsStreaming(true)
  }, [closeStream])

  const connectionBadge = useMemo(() => {
    if (connecting) {
      return { label: "Connecting…", tone: "warn" }
    }
    if (connected) {
      return { label: "Live (SSE)", tone: "ok" }
    }
    if (isStreaming) {
      return { label: "Reconnecting", tone: "warn" }
    }
    return { label: "Paused", tone: "idle" }
  }, [connected, connecting, isStreaming])

  return (
    <div className="flow-logs-view">
      <div className="flow-logs-header">
        <div className="flow-logs-title">
          <div className="flow-logs-name">
            <ScrollText size={14} />
            Flow logs
          </div>
          <p className="flow-logs-subtitle">
            Live logs from the Lin daemon on your machine (default 127.0.0.1:9050).
          </p>
        </div>
        <div className="flow-logs-actions">
          <button className="ghost-button" onClick={fetchHistory} disabled={historyLoading}>
            <RefreshCw size={12} className={historyLoading ? "spin" : ""} />
            <span>Load history</span>
          </button>
          <button className="ghost-button" onClick={handleReconnect} disabled={connecting}>
            <RefreshCw size={12} />
            <span>Reconnect</span>
          </button>
          <button
            className={`ghost-button ${isStreaming ? "active" : ""}`}
            onClick={() => {
              if (isStreaming) {
                closeStream()
                setIsStreaming(false)
              } else {
                setIsStreaming(true)
                setStreamSession((session) => session + 1)
              }
            }}
          >
            {isStreaming ? <Pause size={12} /> : <Play size={12} />}
            <span>{isStreaming ? "Pause" : "Resume"}</span>
          </button>
          <button className="ghost-button" onClick={() => setLogs([])}>
            <TimerReset size={12} />
            <span>Clear</span>
          </button>
        </div>
      </div>

      <div className="flow-logs-status">
        <span className="flow-logs-chip">Base URL: {normalizedBaseUrl}</span>
        <span className={`flow-logs-pill ${connectionBadge.tone}`}>
          <Radio size={12} className={connected ? "pulse" : ""} />
          {connectionBadge.label}
        </span>
        {lastEvent ? (
          <span className="flow-logs-chip">
            Last event {new Date(lastEvent).toLocaleTimeString()}
          </span>
        ) : null}
        {historyLoading ? (
          <span className="flow-logs-chip">
            <RefreshCw size={12} className="spin" />
            Prefilling…
          </span>
        ) : null}
      </div>

      {error ? (
        <div className="flow-logs-warning">
          <AlertTriangle size={14} />
          <div>{error}</div>
        </div>
      ) : null}

      <div className="flow-logs-controls">
        <div className="flow-logs-control-group">
          <label className="flow-logs-label">Host</label>
          <input
            className="flow-logs-input"
            value={host}
            onChange={(event) => setHost(event.target.value)}
            placeholder={DEFAULT_HOST}
          />
        </div>
        <div className="flow-logs-control-group">
          <label className="flow-logs-label">Port</label>
          <input
            className="flow-logs-input"
            type="number"
            min={1}
            max={65535}
            value={port}
            onChange={(event) => setPort(event.target.value)}
            placeholder={DEFAULT_PORT.toString()}
          />
        </div>
        <div className="flow-logs-control-group">
          <label className="flow-logs-label">Server</label>
          <select
            className="flow-logs-select"
            value={selectedServer}
            onChange={(event) => setSelectedServer(event.target.value)}
            disabled={loadingServers}
          >
            {serverOptions.map((opt) => (
              <option key={opt.value} value={opt.value}>
                {opt.label}
              </option>
            ))}
          </select>
        </div>
        <div className="flow-logs-control-group">
          <label className="flow-logs-label">Prefill</label>
          <input
            className="flow-logs-input"
            type="number"
            min={1}
            max={5000}
            value={prefillLimit}
            onChange={(event) => setPrefillLimit(Math.max(1, Number(event.target.value) || 1))}
          />
        </div>
        <div className="flow-logs-toggle">
          <input
            type="checkbox"
            checked={autoScroll}
            onChange={(event) => setAutoScroll(event.target.checked)}
          />
          <span>Auto-scroll</span>
        </div>
        <button
          className="ghost-button"
          onClick={() => {
            fetchHistory()
            setStreamSession((session) => session + 1)
          }}
        >
          <RefreshCw size={12} />
          <span>Apply</span>
        </button>
      </div>

      <div className="flow-logs-grid">
        <div className="flow-logs-stream">
          <div className="flow-logs-stream-header">
            <span>Live logs {selectedServer === "all" ? "(all servers)" : `(${selectedServer})`}</span>
            {isStreaming ? (
              <span className="flow-logs-pill ok">
                <Radio size={12} className={connected ? "pulse" : ""} />
                {connected ? "Streaming" : "Reconnecting"}
              </span>
            ) : (
              <span className="flow-logs-pill idle">Paused</span>
            )}
          </div>
          <div ref={logContainerRef} className="flow-logs-stream-body mono">
            {logs.length === 0 ? (
              <div className="flow-logs-empty">
                No log lines yet. Keep the Lin daemon running and we will stream here.
              </div>
            ) : (
              <div className="flow-logs-rows">
                {logs.map((log, idx) => {
                  const time = formatTime(log.timestamp_ms)
                  const line = stringifyLine(log.line)
                  return (
                    <div key={`${log.server}-${log.timestamp_ms}-${idx}`} className="flow-logs-row">
                      <span className="flow-logs-time">{time}</span>
                      <span className="flow-logs-tag">{log.server}</span>
                      <span
                        className={`flow-logs-stream-tag ${
                          log.stream === "stderr" ? "err" : "out"
                        }`}
                      >
                        {log.stream}
                      </span>
                      <span className="flow-logs-message">{line || " "}</span>
                    </div>
                  )
                })}
              </div>
            )}
          </div>
        </div>

        <div className="flow-logs-side">
          <CommandList
            commands={commands}
            loading={commandsLoading}
            error={commandsError}
            onRefresh={refresh}
            logDir={logDir}
            metaDir={metaDir}
          />
          <FlowHistoryCard
            entries={historyEntries}
            loading={historyFileLoading}
            error={historyError}
            onReload={loadFlowHistory}
            filePath={historyFilePath}
          />
        </div>
      </div>
    </div>
  )
}

export default FlowLogsView
