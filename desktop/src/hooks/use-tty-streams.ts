import { homeDir, join } from "@tauri-apps/api/path"
import {
  exists,
  type FileHandle,
  open,
  readDir,
  SeekMode,
  stat,
  type UnwatchFn,
  watchImmediate,
} from "@tauri-apps/plugin-fs"
import { useCallback, useEffect, useMemo, useRef, useState } from "react"

const MAX_LINES = 400
const MAX_COMMANDS = 60
const INITIAL_BACKFILL_BYTES = 64_000
const META_BACKFILL_BYTES = 24_000
const LIVE_THRESHOLD_MS = 3_500

// Terminal escape sequence parsing - control characters are intentional
// biome-ignore lint/suspicious/noControlCharactersInRegex: Terminal escape sequences require control characters
const CSI_PATTERN =
  /[\x1b\x9b][[\]()#;?]*(?:(?:[0-9]{1,4}(?:;[0-9]{0,4})*)?[0-9A-ORZcf-nqry=><~])/g
// biome-ignore lint/suspicious/noControlCharactersInRegex: Terminal escape sequences require control characters
const OSC_PATTERN = /\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g
// biome-ignore lint/suspicious/noControlCharactersInRegex: Terminal escape sequences require control characters
const DCS_PATTERN = /\x1b[P^_][\s\S]*?\x1b\\/g
// biome-ignore lint/suspicious/noControlCharactersInRegex: Terminal escape sequences require control characters
const CONTROL_PATTERN = /[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g

export type SessionStatus = "live" | "idle" | "missing"

export interface CommandSpan {
  id: string
  startedAt: number
  endedAt?: number
  cwd?: string
  command?: string
  status?: number | null
  sessionId?: string
  output?: string[]
}

export interface SessionView {
  id: string
  logPath: string
  metaPath?: string | null
  status: SessionStatus
  lines: string[]
  lastUpdated: number
  lastModified?: number
  truncatedFrom?: number
  commands: CommandSpan[]
}

export interface CommandView {
  id: string
  sessionId: string
  logPath: string
  startedAt: number
  endedAt?: number
  cwd?: string
  command?: string
  status?: number | null
  output: string[]
}

interface SessionRuntime {
  id: string
  logPath: string
  metaPath?: string | null
  lines: string[]
  commands: CommandSpan[]
  commandOutputs: Map<string, string[]>
  currentCommandId?: string
  commandRemainder: string
  logHandle?: FileHandle
  metaHandle?: FileHandle
  logHandlePath?: string
  metaHandlePath?: string
  logOffset: number
  metaOffset: number
  logRemainder: string
  metaRemainder: string
  truncatedFrom?: number
  lastUpdated: number
  lastModified?: number
  state: SessionStatus
}

interface ParsedMetaLine {
  type: "start" | "end"
  timestamp: number
  commandId: string
  cwd?: string
  command?: string
  status?: number | null
}

interface TTYOptions {
  baseDir?: string
  logDirName?: string
  metaDirName?: string
  label?: string
}

interface UseTTYStreamsResult {
  sessions: SessionView[]
  commands: CommandView[]
  loading: boolean
  error?: string | null
  logDir?: string
  metaDir?: string
  refresh: () => Promise<void>
}

const textDecoder = new TextDecoder()

const sanitizeTTYLine = (line: string) =>
  line
    .replace(OSC_PATTERN, "")
    .replace(DCS_PATTERN, "")
    .replace(CSI_PATTERN, "")
    .replace(CONTROL_PATTERN, "")

const trimLines = (lines: string[]) => (lines.length > MAX_LINES ? lines.slice(-MAX_LINES) : lines)

const trimCommands = (commands: CommandSpan[]) =>
  commands.length > MAX_COMMANDS ? commands.slice(0, MAX_COMMANDS) : commands

const parseLogId = (name: string) => {
  const trimmed = name.replace(/\.log$/, "")
  if (!trimmed) return null
  const sessionMatch = /^session-([A-Za-z0-9-]+)/.exec(trimmed)
  if (sessionMatch) return sessionMatch[1]
  return trimmed
}

const parseMetaId = (name: string) => {
  const match = /^([A-Za-z0-9-_%]+)\.log$/.exec(name)
  return match ? match[1] : null
}

const decodeBase64Safe = (value?: string) => {
  if (!value) return undefined

  try {
    return atob(value)
  } catch {
    return undefined
  }
}

const parseMetaLine = (line: string): ParsedMetaLine | null => {
  const trimmed = line.trim()
  if (!trimmed) return null

  const parts = trimmed.split(" ")
  const kind = parts[0]
  const timestamp = Number(parts[1])
  const commandId = parts[2]

  if (!kind || Number.isNaN(timestamp) || !commandId) {
    return null
  }

  if (kind === "start") {
    return {
      type: "start",
      timestamp,
      commandId,
      cwd: decodeBase64Safe(parts[3]),
      command: decodeBase64Safe(parts[4]),
    }
  }

  if (kind === "end") {
    const status = parts[3]
    const parsedStatus = status === undefined ? undefined : Number(status)
    return {
      type: "end",
      timestamp,
      commandId,
      status: Number.isNaN(parsedStatus) ? null : parsedStatus,
    }
  }

  return null
}

const applyMetaEvents = (session: SessionRuntime, events: ParsedMetaLine[]) => {
  let changed = false

  for (const event of events) {
    let command = session.commands.find((entry) => entry.id === event.commandId)
    if (!command) {
      command = {
        id: event.commandId,
        startedAt: event.timestamp,
        sessionId: session.id,
      }
      session.commands.push(command)
    }

    if (event.type === "start") {
      command.startedAt = event.timestamp
      command.cwd = event.cwd ?? command.cwd
      command.command = event.command ?? command.command
      command.endedAt = undefined
      command.status = undefined
    } else {
      command.endedAt = event.timestamp
      command.status = event.status ?? null
      if (!command.startedAt) {
        command.startedAt = event.timestamp
      }
    }

    changed = true
  }

  if (changed) {
    session.commands.sort((a, b) => (b.startedAt ?? 0) - (a.startedAt ?? 0))
    session.commands = trimCommands(session.commands)
  }

  return changed
}

const readIncrementalText = async (
  handle: FileHandle,
  offset: number,
  limitBytes: number,
  isInitial: boolean,
  remainder: string,
) => {
  const stats = await handle.stat()
  const size = stats.size ?? 0

  let start = offset
  if (isInitial && size > limitBytes) {
    start = size - limitBytes
  }

  if (start < 0) start = 0

  if (start === offset && start >= size) {
    return { text: "", offset, remainder, truncatedFrom: undefined }
  }

  const readSize = Math.min(size - start, limitBytes)
  await handle.seek(start, SeekMode.Start)
  const bytes = await handle.read(readSize)
  const text = remainder + textDecoder.decode(bytes)

  const nextOffset = start + bytes.length
  let truncatedFrom: number | undefined
  if (isInitial && start > 0) {
    truncatedFrom = start
  }

  return {
    text,
    offset: nextOffset,
    remainder: "",
    truncatedFrom,
  }
}

const readLines = (text: string, remainder: string) => {
  const combined = remainder + text
  const lines = combined.split(/\r?\n/)
  const nextRemainder = lines.pop() ?? ""
  return { lines, remainder: nextRemainder }
}

const readAndSplitLines = async (
  handle: FileHandle,
  offset: number,
  limitBytes: number,
  isInitial: boolean,
  remainder: string,
) => {
  const { text, offset: nextOffset, truncatedFrom, remainder: carry } =
    await readIncrementalText(handle, offset, limitBytes, isInitial, remainder)
  if (!text && carry === remainder) {
    return { lines: [], offset: nextOffset, remainder, truncatedFrom }
  }
  const { lines, remainder: nextRemainder } = readLines(text, carry)
  return {
    lines,
    offset: nextOffset,
    remainder: nextRemainder,
    truncatedFrom,
  }
}

export const useTTYStreams = (options?: TTYOptions): UseTTYStreamsResult => {
  const [sessions, setSessions] = useState<SessionView[]>([])
  const [commands, setCommands] = useState<CommandView[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [paths, setPaths] = useState<{ logDir?: string; metaDir?: string }>({})

  const runtimeRef = useRef<Map<string, SessionRuntime>>(new Map())
  const watcherRef = useRef<UnwatchFn[]>([])
  const drainLockRef = useRef(false)
  const drainQueuedRef = useRef(false)
  const logDirRef = useRef<string>()
  const metaDirRef = useRef<string>()

  const closeHandle = useCallback(async (handle?: FileHandle) => {
    if (!handle) return
    try {
      await handle.close()
    } catch {
      // Ignore close failures
    }
  }, [])

  const ensureHandle = useCallback(
    async (
      session: SessionRuntime,
      kind: "log" | "meta",
      path: string,
    ): Promise<FileHandle | undefined> => {
      const storedPath = kind === "log" ? session.logHandlePath : session.metaHandlePath
      if (storedPath && storedPath !== path) {
        await closeHandle(kind === "log" ? session.logHandle : session.metaHandle)
        if (kind === "log") {
          session.logHandle = undefined
          session.logHandlePath = undefined
        } else {
          session.metaHandle = undefined
          session.metaHandlePath = undefined
        }
      }

      const existing = kind === "log" ? session.logHandle : session.metaHandle
      if (existing) return existing

      try {
        const handle = await open(path, { read: true })
        if (kind === "log") {
          session.logHandle = handle
          session.logHandlePath = path
        } else {
          session.metaHandle = handle
          session.metaHandlePath = path
        }
        return handle
      } catch {
        return undefined
      }
    },
    [closeHandle],
  )

  const publishSessions = useCallback(() => {
    const data = Array.from(runtimeRef.current.values())
      .map((session) => ({
        id: session.id,
        logPath: session.logPath,
        metaPath: session.metaPath,
        status: session.state,
        lines: trimLines(session.lines),
        commands: trimCommands(session.commands),
        lastUpdated: session.lastUpdated,
        lastModified: session.lastModified,
        truncatedFrom: session.truncatedFrom,
      }))
      .sort((a, b) => (b.lastUpdated ?? 0) - (a.lastUpdated ?? 0))

    setSessions(data)

    const allCommands = data
      .flatMap((session) =>
        (session.commands ?? []).map((command) => ({
          id: command.id,
          sessionId: session.id,
          logPath: session.logPath,
          startedAt: command.startedAt,
          endedAt: command.endedAt,
          cwd: command.cwd,
          command: command.command,
          status: command.status,
          output: command.output ?? [],
        })),
      )
      .sort((a, b) => (b.startedAt ?? 0) - (a.startedAt ?? 0))
      .slice(0, MAX_COMMANDS)

    setCommands(allCommands)
  }, [])

  const refreshSessions = useCallback(async () => {
    const logDir = logDirRef.current
    if (!logDir) return
    let files: string[] = []

    try {
      const entries = await readDir(logDir)
      files = entries
        .filter((entry) => entry.isFile && entry.name)
        .map((entry) => entry.name as string)
    } catch {
      files = []
    }

    const nextIds = new Set<string>()
    for (const name of files) {
      const id = parseLogId(name)
      if (!id) continue
      nextIds.add(id)

      if (!runtimeRef.current.has(id)) {
        runtimeRef.current.set(id, {
          id,
          logPath: await join(logDir, name),
          metaPath: null,
          lines: [],
          commands: [],
          commandOutputs: new Map(),
          commandRemainder: "",
          logOffset: 0,
          metaOffset: 0,
          logRemainder: "",
          metaRemainder: "",
          lastUpdated: 0,
          state: "idle",
        })
      }
    }

    for (const id of runtimeRef.current.keys()) {
      if (!nextIds.has(id)) {
        const session = runtimeRef.current.get(id)
        if (session) {
          await closeHandle(session.logHandle)
          await closeHandle(session.metaHandle)
        }
        runtimeRef.current.delete(id)
      }
    }
  }, [closeHandle])

  const refreshMeta = useCallback(async () => {
    const metaDir = metaDirRef.current
    if (!metaDir) return

    let entries: string[] = []
    try {
      const files = await readDir(metaDir)
      entries = files
        .filter((entry) => entry.isFile && entry.name)
        .map((entry) => entry.name as string)
    } catch {
      entries = []
    }

    for (const name of entries) {
      const id = parseMetaId(name)
      if (!id) continue
      const runtime = runtimeRef.current.get(id)
      if (!runtime) continue
      runtime.metaPath = await join(metaDir, name)
    }
  }, [])

  const drainSessions = useCallback(
    async (initial: boolean) => {
      if (drainLockRef.current) {
        drainQueuedRef.current = true
        return
      }
      drainLockRef.current = true

      let mutated = false

      try {
        await refreshMeta()

        for (const session of runtimeRef.current.values()) {
          let logHandle = session.logHandle
          let metaHandle = session.metaHandle
          if (session.logPath) {
            logHandle = await ensureHandle(session, "log", session.logPath)
          }
          if (session.metaPath) {
            metaHandle = await ensureHandle(session, "meta", session.metaPath)
          }

          if (logHandle) {
            const { lines, offset, remainder, truncatedFrom } = await readAndSplitLines(
              logHandle,
              session.logOffset,
              INITIAL_BACKFILL_BYTES,
              initial,
              session.logRemainder,
            )

            if (lines.length) {
              session.logOffset = offset
              session.logRemainder = remainder
              session.truncatedFrom = truncatedFrom ?? session.truncatedFrom
              const sanitized = lines.map((line) => sanitizeTTYLine(line))
              session.lines = trimLines([...session.lines, ...sanitized])
              session.lastUpdated = Date.now()
              session.state = Date.now() - session.lastUpdated < LIVE_THRESHOLD_MS ? "live" : "idle"
              mutated = true
            }
          }

          if (metaHandle) {
            try {
              const metaStats = await metaHandle.stat()
              const size = metaStats.size ?? 0
              const isInitial = initial || session.metaOffset === 0
              const metaRead = await readAndSplitLines(
                metaHandle,
                session.metaOffset,
                META_BACKFILL_BYTES,
                isInitial,
                session.metaRemainder,
              )
              if (metaRead.lines.length) {
                session.metaOffset = metaRead.offset
                session.metaRemainder = metaRead.remainder
                const events = metaRead.lines
                  .map((line) => parseMetaLine(line))
                  .filter(Boolean) as ParsedMetaLine[]
                if (events.length) {
                  const changed = applyMetaEvents(session, events)
                  if (changed) {
                    session.lastUpdated = Date.now()
                    mutated = true
                  }
                }
              }

              if (size === 0) {
                session.metaOffset = 0
              }
            } catch {
              // ignore meta read errors
            }
          }
        }
      } finally {
        drainLockRef.current = false
      }

      if (mutated) {
        publishSessions()
      }

      if (drainQueuedRef.current) {
        drainQueuedRef.current = false
        void drainSessions(initial)
      }
    },
    [ensureHandle, publishSessions, refreshMeta],
  )

  const handleFsEvent = useCallback(async () => {
    await refreshSessions()
    await drainSessions(false)
  }, [drainSessions, refreshSessions])

  useEffect(() => {
    let disposed = false

    const initialize = async () => {
      try {
        const home = await homeDir()
        const base = options?.baseDir ?? ".lin"
        const logDirName = options?.logDirName ?? "tty-logs"
        const metaDirName = options?.metaDirName ?? "tty-meta"
        const label = options?.label ?? "lin"

        const logDir = await join(home, base, logDirName)
        const metaDir = await join(home, base, metaDirName)

        logDirRef.current = logDir
        metaDirRef.current = metaDir
        setPaths({ logDir, metaDir })

        const hasLogs = await exists(logDir).catch(() => false)
        if (!hasLogs) {
          setError(
            `TTY logs not found at ${logDir}. Start a ${label} session or enable trace_terminal_io.`,
          )
          setLoading(false)
          return
        }

        await refreshSessions()
        await drainSessions(true)

        const unwatchLogs = await watchImmediate(logDir, handleFsEvent, { recursive: false })
        watcherRef.current.push(unwatchLogs)

        if (await exists(metaDir).catch(() => false)) {
          const unwatchMeta = await watchImmediate(metaDir, handleFsEvent, { recursive: false })
          watcherRef.current.push(unwatchMeta)
        }

        setError(null)
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err)
        console.error("Failed to initialize tty streams", err)
        setError(message)
      } finally {
        if (!disposed) {
          setLoading(false)
        }
      }
    }

    void initialize()
    const interval = setInterval(() => {
      void drainSessions(false)
    }, 1_200)

    return () => {
      disposed = true
      clearInterval(interval)
      for (const unwatch of watcherRef.current) {
        try {
          unwatch()
        } catch (err) {
          console.warn("Failed to unwatch tty logs", err)
        }
      }
      watcherRef.current = []

      for (const session of runtimeRef.current.values()) {
        void closeHandle(session.logHandle)
        void closeHandle(session.metaHandle)
      }
      runtimeRef.current.clear()
    }
  }, [
    closeHandle,
    drainSessions,
    handleFsEvent,
    refreshSessions,
    options?.baseDir,
    options?.logDirName,
    options?.metaDirName,
    options?.label,
  ])

  const refresh = useCallback(async () => {
    await refreshSessions()
    await drainSessions(false)
  }, [drainSessions, refreshSessions])

  const result = useMemo<UseTTYStreamsResult>(
    () => ({
      sessions,
      commands,
      loading,
      error,
      logDir: paths.logDir,
      metaDir: paths.metaDir,
      refresh,
    }),
    [sessions, commands, loading, error, paths.logDir, paths.metaDir, refresh],
  )

  return result
}
