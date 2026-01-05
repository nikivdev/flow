import { Children, type ReactNode, useCallback, useEffect, useMemo, useRef, useState } from "react"
import { invoke } from "@tauri-apps/api/core"
import { open as openDialog } from "@tauri-apps/plugin-dialog"
import { openPath } from "@tauri-apps/plugin-opener"
import type { PluggableList } from "unified"
import { Streamdown, defaultRehypePlugins, defaultRemarkPlugins } from "streamdown"

const TOOL_LINE_PREFIXES = ["[tool_use]", "[tool_result]", "[thinking]"]
const ROOTS_STORAGE_KEY = "flow.desktop.roots"

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

const App = () => {
  const [projects, setProjects] = useState<DesktopProject[]>([])
  const [roots, setRoots] = useState<string[]>([])
  const [selectedProjectRoot, setSelectedProjectRoot] = useState<string | null>(null)
  const [sessions, setSessions] = useState<SessionWithProject[]>([])
  const [logs, setLogs] = useState<StoredLogEntry[]>([])
  const [view, setView] = useState<"sessions" | "logs">("sessions")
  const [expandedSessions, setExpandedSessions] = useState<Set<string>>(new Set())
  const [paletteOpen, setPaletteOpen] = useState(false)
  const [loadingSessions, setLoadingSessions] = useState(false)
  const [loadingLogs, setLoadingLogs] = useState(false)

  const selectedProject = useMemo(() => {
    return projects.find((project) => project.project_root === selectedProjectRoot) ?? null
  }, [projects, selectedProjectRoot])

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
    const handleKey = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault()
        setPaletteOpen(true)
      }
    }
    window.addEventListener("keydown", handleKey)
    return () => window.removeEventListener("keydown", handleKey)
  }, [])

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
            <h2>{selectedProject?.name ?? "All projects"}</h2>
            <div className="meta">
              {selectedProject ? shortenPath(selectedProject.project_root) : "Watching every flow.toml you added"}
            </div>
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
          ) : (
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
          )}
        </section>
      </main>

      <CommandPalette open={paletteOpen} actions={commandActions} onClose={() => setPaletteOpen(false)} />
    </div>
  )
}

export default App
