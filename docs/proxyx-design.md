# proxyx: Zero-Cost Traced Reverse Proxy for Flow

A lightweight reverse proxy with always-on observability, designed for macOS development.

## Design Principles

1. **Zero-cost tracing** - mmap ring buffer, no allocations per request
2. **Flow-native** - integrates with `f` commands and flow.toml
3. **AI-powered naming** - suggests proxy names from port/process info
4. **macOS-optimized** - no Docker/K8s complexity
5. **Dev-time intelligence** - traces inform AI agents writing code

---

## How Traces Help During Development (Rise Example)

Rise has multiple services that need coordination during development:

```
┌─────────────────────────────────────────────────────────────────────┐
│ Development Session                                                  │
│                                                                      │
│  Claude Code ◄──── reads traces ────► proxyx ring buffer            │
│       │                                     ▲                        │
│       │ writes code                         │ records all requests   │
│       ▼                                     │                        │
│  ┌─────────┐    ┌─────────┐    ┌───────────┴───────────┐            │
│  │ web     │───▶│ daemon  │───▶│ zai/xai/cerebras/etc │            │
│  │ :5173   │    │ :7654   │    └───────────────────────┘            │
│  └─────────┘    └─────────┘                                         │
│       │              │                                               │
│       │              ▼                                               │
│       │         ┌─────────┐                                         │
│       └────────▶│ api     │                                         │
│                 │ :8787   │                                         │
│                 └─────────┘                                         │
└─────────────────────────────────────────────────────────────────────┘
```

### Use Case 1: "Why is my AI call slow?"

When you're writing code and notice latency, instead of hunting through logs:

```bash
f proxy last --target daemon
# Request 3f2a:
#   Path: /v1/chat/completions
#   Provider: zai → cerebras (fallback)
#   Latency: 4200ms (provider: 4150ms, overhead: 50ms)
#   Tokens: 1200 in, 340 out
#   Error: zai timeout after 3000ms, retried cerebras
```

**AI agent can read this** and suggest: "zai is timing out - switch default provider to cerebras in your session"

### Use Case 2: "What API calls does this component make?"

When editing a component, see exactly what requests it triggers:

```bash
f proxy trace --since "10s ago" --source web
# TIME        METHOD  PATH                STATUS  LATENCY  TARGET
# 14:32:01    POST    /v1/chat/completions  200    120ms   daemon
# 14:32:01    GET     /api/user/profile     200    8ms     api
# 14:32:02    POST    /api/mutations        200    45ms    api
```

**AI agent can correlate**: "Your UserProfile component makes 3 requests on mount - the chat completion is redundant here"

### Use Case 3: "Debug failing mutation"

When Effect mutation fails, trace shows the full picture:

```bash
f proxy trace --errors --last 5
# Request a3f1:
#   Path: /api/mutations
#   Status: 500
#   Upstream response: {"_tag":"ParseError","message":"Expected string at path.name"}
#   Request body hash: 0x3f2a... (see f proxy body a3f1)
```

**AI agent sees**: typed error from Effect schema validation, can fix the code directly

### Use Case 4: "Correlation across services"

Trace ID propagates through all services:

```bash
f proxy trace --id abc123
# Trace abc123 (total: 340ms):
#   14:32:01.000  web → daemon    POST /v1/chat/completions  (started)
#   14:32:01.050  daemon → zai    POST /chat/completions     (timeout 3000ms)
#   14:32:04.050  daemon → cerebras POST /chat/completions   (fallback)
#   14:32:04.200  cerebras → daemon 200 OK                   (150ms)
#   14:32:04.210  daemon → web    200 OK                     (streaming start)
#   14:32:04.340  daemon → web    streaming complete         (340ms total)
```

### Use Case 5: "What changed between working and broken?"

Compare request patterns before/after a code change:

```bash
f proxy diff --before "5 min ago" --after "now"
# New requests:
#   + POST /api/mutations (didn't exist before)
# Changed requests:
#   ~ GET /api/user/profile: added header X-Cache-Bust
# Missing requests:
#   - GET /api/user/settings (no longer called)
```

---

## Integration with Claude Code / AI Agents

The key insight: **traces are structured data AI agents can consume**.

```rust
// In Claude Code's context, expose trace summary
pub struct TraceContext {
    pub recent_errors: Vec<TraceRecord>,      // Last 5 errors
    pub slow_requests: Vec<TraceRecord>,      // p99 > 500ms
    pub request_patterns: HashMap<String, u32>, // Path -> count
    pub provider_health: HashMap<String, ProviderStats>,
}

impl TraceContext {
    /// Called by AI agent to understand current state
    pub fn summarize(&self) -> String {
        format!(
            "Recent errors: {}\nSlow requests: {}\nMost called: {}",
            self.recent_errors.len(),
            self.slow_requests.len(),
            self.request_patterns.iter().max_by_key(|&(_, v)| v).map(|(k, _)| k).unwrap_or("none")
        )
    }
}
```

### Agent-Readable Trace File

In addition to binary ring buffer, write agent-friendly summary:

```
~/.config/flow/proxy/trace-summary.json
{
  "last_updated": 1706000000,
  "session": {
    "started": 1705990000,
    "requests": 1234,
    "errors": 5,
    "avg_latency_ms": 45
  },
  "recent_errors": [
    {
      "time": "14:32:01",
      "path": "/api/mutations",
      "status": 500,
      "error": "ParseError: Expected string at path.name",
      "suggestion": "Check schema validation in mutations endpoint"
    }
  ],
  "slow_requests": [
    {
      "time": "14:31:45",
      "path": "/v1/chat/completions",
      "latency_ms": 4200,
      "reason": "Provider fallback: zai → cerebras"
    }
  ],
  "provider_status": {
    "zai": { "healthy": false, "last_error": "timeout", "error_rate": "40%" },
    "cerebras": { "healthy": true, "avg_latency_ms": 150 }
  }
}
```

Claude Code can read this file and proactively suggest fixes:

> "I notice zai has a 40% error rate in the last 5 minutes. Should I switch your default provider to cerebras?"

---

## Rise-Specific Configuration

```toml
# ~/code/rise/flow.toml

[proxy]
trace_summary = true  # Write agent-readable JSON summary
trace_interval = "1s" # Update summary every second

[[proxies]]
name = "daemon"
target = "localhost:7654"
# Capture request/response bodies for AI endpoints
capture_body = true
capture_body_max = "64KB"

[[proxies]]
name = "api"
target = "localhost:8787"
# Correlate with Effect trace events
effect_trace_header = "X-Trace-Id"

[[proxies]]
name = "web"
target = "localhost:5173"
# Don't capture static assets
exclude_paths = ["/assets/*", "*.js", "*.css"]
```

## Integration with Rise's Existing Tracing

Rise already has two tracing mechanisms:

1. **Daemon logs** (`/logs` endpoint) - in-memory, lost on restart
2. **Effect Trace service** - writes to JSONL file + HTTP endpoint

proxyx unifies these by:

1. **Intercepting all HTTP** - captures what daemon logs miss (non-AI requests)
2. **Correlating trace IDs** - links Effect mutations to HTTP requests
3. **Persisting in ring buffer** - survives restarts, zero-cost
4. **Exposing to AI agents** - structured summary for Claude Code

```
Before (fragmented):
  web → daemon (logged in daemon memory, lost on restart)
  web → api (logged in Effect Trace JSONL, separate file)
  No correlation between them

After (unified):
  web → proxyx → daemon (ring buffer + summary JSON)
       └──────→ api    (ring buffer + summary JSON)

  All requests correlated by trace ID, readable by AI agents
```

### Trace ID Propagation

proxyx generates trace IDs and propagates them:

```
Request from web:
  → proxyx adds X-Trace-Id: abc123 (if not present)
  → forwards to daemon with X-Trace-Id: abc123
  → daemon logs include trace_id: abc123
  → Effect Trace service receives X-Trace-Id header
  → All logs correlate to abc123
```

This means `f proxy trace --id abc123` shows the complete journey.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│ flow.toml                                                            │
│                                                                      │
│ [[proxies]]                                                          │
│ name = "api"                    # AI can suggest this                │
│ listen = ":8080"                # or auto-assign                     │
│ target = "localhost:3000"                                            │
│ host = "api.local"              # optional host-based routing        │
│                                                                      │
│ [[proxies]]                                                          │
│ name = "docs"                                                        │
│ target = "localhost:4000"                                            │
└─────────────────────────────────────────────────────────────────────┘
                                    │
                                    ▼
┌─────────────────────────────────────────────────────────────────────┐
│ proxyx daemon (spawned by flow supervisor)                           │
│                                                                      │
│  ┌──────────────┐    ┌──────────────┐    ┌────────────────────────┐ │
│  │ Listener     │───▶│ Router       │───▶│ Backend Pool           │ │
│  │ (hyper)      │    │ (path/host)  │    │ (crossbeam queue)      │ │
│  └──────────────┘    └──────────────┘    └────────────────────────┘ │
│         │                                          │                 │
│         │            ┌──────────────────────────────┘                │
│         ▼            ▼                                               │
│  ┌─────────────────────────────────────────────────────────────────┐│
│  │ Trace Ring Buffer (mmap)                                         ││
│  │ ~/.config/flow/proxy/trace.<pid>.bin                             ││
│  │                                                                  ││
│  │ Header (64 bytes):                                               ││
│  │   magic: "PROXYTRC"                                              ││
│  │   version: 1                                                     ││
│  │   capacity: N                                                    ││
│  │   write_index: AtomicU64                                         ││
│  │                                                                  ││
│  │ Records (128 bytes each):                                        ││
│  │   [ts_ns, req_id, method, status, latency_us,                   ││
│  │    bytes_in, bytes_out, target_idx, path_hash, path_prefix]     ││
│  └─────────────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────────┘
```

## Trace Record Structure

```rust
const TRACE_MAGIC: &[u8; 8] = b"PROXYTRC";
const TRACE_VERSION: u32 = 1;
const TRACE_RECORD_SIZE: usize = 128;
const TRACE_PATH_BYTES: usize = 64;

#[repr(C)]
struct TraceHeader {
    magic: [u8; 8],
    version: u32,
    record_size: u32,
    capacity: u64,
    write_index: AtomicU64,
    // Target table (name -> index mapping written at init)
    target_count: u32,
    _reserved: [u8; 28],
}

#[repr(C)]
struct TraceRecord {
    ts_ns: u64,           // Monotonic timestamp
    req_id: u64,          // Unique request ID (atomic counter)
    method: u8,           // 1=GET, 2=POST, 3=PUT, 4=DELETE, etc.
    status: u16,          // HTTP status code
    _pad: u8,
    latency_us: u32,      // Response time in microseconds
    bytes_in: u32,        // Request body size
    bytes_out: u32,       // Response body size
    target_idx: u8,       // Index into target table
    path_len: u8,
    _pad2: [u8; 2],
    path_hash: u64,       // FNV-1a hash of full path
    path: [u8; 64],       // Path prefix (truncated if longer)
    client_ip: [u8; 16],  // IPv4 (4 bytes) or IPv6 (16 bytes)
    upstream_latency_us: u32,
    _reserved: [u8; 4],
}
```

## Components

### 1. Proxy Core (Pingora-inspired)

```rust
// Simplified from Pingora's ProxyHttp trait
pub trait ProxyHandler: Send + Sync {
    /// Called before forwarding request
    fn request_filter(&self, req: &mut Request, ctx: &mut ProxyCtx) -> Result<()> {
        Ok(())
    }

    /// Select upstream target
    fn upstream_peer(&self, req: &Request, ctx: &mut ProxyCtx) -> Result<&Backend>;

    /// Called after receiving response
    fn response_filter(&self, resp: &mut Response, ctx: &mut ProxyCtx) -> Result<()> {
        Ok(())
    }

    /// Called after request completes (success or failure)
    fn logging(&self, req: &Request, resp: Option<&Response>, ctx: &ProxyCtx) {
        // Default: write to trace ring buffer
    }
}

pub struct ProxyCtx {
    pub req_id: u64,
    pub start_time: Instant,
    pub upstream_connect_time: Option<Duration>,
    pub target_idx: u8,
}
```

### 2. Connection Pool (Pingora-inspired, simplified)

```rust
use crossbeam::queue::ArrayQueue;

pub struct ConnectionPool {
    // Lock-free queue for hot connections (sized for dev workloads)
    hot: ArrayQueue<PooledConnection>,
    // Target address
    addr: SocketAddr,
    // Pool stats (atomic counters)
    reused: AtomicU64,
    created: AtomicU64,
}

impl ConnectionPool {
    pub fn new(addr: SocketAddr, capacity: usize) -> Self {
        Self {
            hot: ArrayQueue::new(capacity),
            addr,
            reused: AtomicU64::new(0),
            created: AtomicU64::new(0),
        }
    }

    pub async fn get(&self) -> Result<Connection> {
        // Try hot queue first (lock-free)
        if let Some(conn) = self.hot.pop() {
            if conn.is_alive() {
                self.reused.fetch_add(1, Ordering::Relaxed);
                return Ok(conn.into_connection());
            }
        }
        // Create new connection
        self.created.fetch_add(1, Ordering::Relaxed);
        Connection::new(self.addr).await
    }

    pub fn put(&self, conn: Connection) {
        let pooled = PooledConnection::from(conn);
        // Best effort - if queue is full, connection is dropped
        let _ = self.hot.push(pooled);
    }
}
```

### 3. Trace Ring Buffer

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::ptr::write_unaligned;

pub struct TraceBuffer {
    header: *mut TraceHeader,
    records: *mut u8,
    capacity: u64,
    req_counter: AtomicU64,
}

impl TraceBuffer {
    /// Record a completed request (zero allocations)
    #[inline]
    pub fn record(&self, record: &TraceRecord) {
        let idx = unsafe {
            (*self.header).write_index.fetch_add(1, Ordering::Relaxed)
        };
        let slot = (idx % self.capacity) as usize;
        let dst = unsafe {
            self.records.add(slot * TRACE_RECORD_SIZE) as *mut TraceRecord
        };
        unsafe { write_unaligned(dst, *record) };
    }

    /// Get next request ID
    #[inline]
    pub fn next_req_id(&self) -> u64 {
        self.req_counter.fetch_add(1, Ordering::Relaxed)
    }
}
```

### 4. Router

```rust
pub struct Router {
    // Host -> target index
    host_routes: HashMap<String, usize>,
    // Path prefix -> target index
    path_routes: Vec<(String, usize)>,
    // Default target
    default_target: Option<usize>,
    // All backends
    backends: Vec<Backend>,
}

pub struct Backend {
    pub name: String,
    pub addr: SocketAddr,
    pub pool: ConnectionPool,
}

impl Router {
    pub fn route(&self, req: &Request) -> Option<&Backend> {
        // 1. Check host header
        if let Some(host) = req.headers().get("host") {
            if let Some(&idx) = self.host_routes.get(host.to_str().ok()?) {
                return Some(&self.backends[idx]);
            }
        }

        // 2. Check path prefix
        let path = req.uri().path();
        for (prefix, idx) in &self.path_routes {
            if path.starts_with(prefix) {
                return Some(&self.backends[*idx]);
            }
        }

        // 3. Default
        self.default_target.map(|idx| &self.backends[idx])
    }
}
```

## CLI Commands

```bash
# List active proxies
f proxy
# Output:
# NAME    LISTEN      TARGET           REQS    ERRORS  LATENCY(p99)
# api     :8080       localhost:3000   1.2k    0       12ms
# docs    :8081       localhost:4000   340     2       8ms

# Add a proxy (AI suggests name)
f proxy add localhost:3000
# Detected: node process, cwd=/Users/nikiv/code/myapi
# Suggested name: "myapi" [Y/n/custom]:

# Add with explicit name
f proxy add localhost:3000 --name api

# View recent requests
f proxy trace
# Output (tail of ring buffer):
# TIME        REQ_ID  METHOD  PATH           STATUS  LATENCY  TARGET
# 14:32:01    a3f2    GET     /api/users     200     12ms     api
# 14:32:01    a3f3    POST    /api/login     401     8ms      api
# 14:32:02    a3f4    GET     /docs/intro    200     4ms      docs

# View last request details
f proxy last
# Request a3f4:
#   Method: GET
#   Path: /docs/intro
#   Status: 200
#   Latency: 4ms
#   Upstream: 3ms
#   Bytes: 0 in, 4.2KB out

# Follow trace in real-time
f proxy trace -f

# Filter by target
f proxy trace --target api

# Stop proxy daemon
f proxy stop
```

## Flow.toml Schema

```toml
[proxy]
# Global proxy settings
listen = ":8080"              # Default listen address
trace_size = "16MB"           # Ring buffer size
trace_dir = "~/.config/flow/proxy"

[[proxies]]
name = "api"
target = "localhost:3000"
# Optional: host-based routing
host = "api.local"
# Optional: path prefix routing
path = "/api"
# Optional: health check
health = "/health"
health_interval = "10s"

[[proxies]]
name = "docs"
target = "localhost:4000"
path = "/docs"
```

## AI Naming Integration

When `f proxy add <target>` is called without `--name`:

```rust
pub struct PortInfo {
    pub port: u16,
    pub process: Option<String>,      // e.g., "node", "python"
    pub cwd: Option<PathBuf>,         // Process working directory
    pub cmdline: Option<String>,      // Full command line
    pub listening_since: Option<Duration>,
}

pub async fn suggest_proxy_name(info: &PortInfo) -> String {
    // 1. Try to infer from cwd (last path component)
    if let Some(cwd) = &info.cwd {
        if let Some(name) = cwd.file_name() {
            return sanitize_name(name.to_string_lossy());
        }
    }

    // 2. Try to infer from process + port
    if let Some(proc) = &info.process {
        return format!("{}-{}", proc, info.port);
    }

    // 3. Fall back to port
    format!("svc-{}", info.port)
}

// For smarter naming, call LLM with context
pub async fn ai_suggest_name(info: &PortInfo) -> Result<String> {
    let prompt = format!(
        "Suggest a short, memorable name for a local dev proxy:\n\
         Port: {}\n\
         Process: {:?}\n\
         Working dir: {:?}\n\
         Reply with just the name (lowercase, no spaces).",
        info.port, info.process, info.cwd
    );
    // Call local LLM or API
    llm_complete(&prompt).await
}
```

## Implementation Plan

### Phase 1: Core Proxy
1. [ ] `ProxyConfig` struct in flow's config.rs
2. [ ] Trace ring buffer module (`src/proxy/trace.rs`)
3. [ ] Basic hyper-based proxy (`src/proxy/server.rs`)
4. [ ] Router with host/path matching (`src/proxy/router.rs`)
5. [ ] Connection pool (`src/proxy/pool.rs`)

### Phase 2: Flow Integration
1. [ ] `f proxy` subcommands in CLI
2. [ ] Supervisor integration (daemon lifecycle)
3. [ ] Hot reload on flow.toml changes

### Phase 3: AI & Polish
1. [ ] Port scanning for `f proxy add`
2. [ ] AI name suggestion
3. [ ] `f proxy trace` viewer
4. [ ] Health checks

## Dependencies

```toml
# Add to Cargo.toml
hyper = { version = "1", features = ["http1", "http2", "server", "client"] }
hyper-util = { version = "0.1", features = ["tokio"] }
crossbeam = { version = "0.8", features = ["crossbeam-queue"] }
# Already have: tokio, libc, memmap2 (or use libc::mmap directly)
```

## File Structure

```
src/
├── proxy/
│   ├── mod.rs          # Re-exports
│   ├── config.rs       # ProxyConfig parsing
│   ├── server.rs       # Hyper server + request handling
│   ├── router.rs       # Host/path routing
│   ├── pool.rs         # Connection pooling
│   ├── trace.rs        # mmap ring buffer
│   ├── summary.rs      # Agent-readable JSON summary
│   └── ai.rs           # Name suggestion
├── cmd/
│   └── proxy.rs        # CLI commands
└── ...
```

---

## Claude Code Integration (The Key Feature)

The real value: **AI sees your app's behavior while helping you code**.

### CLAUDE.md Hook

Add to your project's CLAUDE.md:

```markdown
## Development Context

When helping with this project, check the proxy trace summary:
- File: ~/.config/flow/proxy/trace-summary.json
- Command: `f proxy last` for recent request details

If you see errors or slow requests, mention them proactively.
```

### Automatic Context Injection

Flow can inject trace context into Claude Code sessions:

```bash
# In flow.toml
[claude]
context_files = [
  "~/.config/flow/proxy/trace-summary.json"
]
```

Now Claude Code automatically sees:
- Recent errors (can fix the code causing them)
- Slow requests (can suggest optimizations)
- Provider health (can suggest fallbacks)
- Request patterns (can identify redundant calls)

### Example Interaction

You: "The user profile page is slow"

Claude (reading trace-summary.json):
> Looking at the trace data, I see UserProfile makes 3 requests on mount:
> 1. GET /api/user/profile (8ms) ✓
> 2. POST /v1/chat/completions (4200ms) ← this is slow
> 3. GET /api/user/settings (12ms) ✓
>
> The chat completion is being called on every mount. Looking at the code...
> this is in useEffect without deps. Should I add caching or move it to user action?

### Real-time Error Notification

When a request fails, Flow can notify Claude Code:

```rust
// In proxyx, when error detected:
if record.status >= 500 {
    // Write to a file Claude Code watches
    write_error_notification(&record);
}
```

Claude Code sees the notification and can proactively say:
> "I just saw a 500 error on POST /api/mutations - the request body had an invalid schema. Want me to fix it?"

---

## Development Workflow with Traces

### Starting Development

```bash
# Terminal 1: Start proxyx (intercepts all traffic)
f proxy start

# Terminal 2: Start Rise services through proxy
f dev  # web, daemon, api all route through proxyx

# Terminal 3: Claude Code
# Claude reads trace-summary.json automatically
```

### While Coding

1. **You edit code** → triggers requests
2. **proxyx records** → updates trace-summary.json
3. **Claude Code reads** → understands what happened
4. **Claude suggests** → "That request failed because..."

### Debugging Session

```bash
# See what's happening right now
f proxy trace -f

# See last error details
f proxy last --errors

# See specific request with body
f proxy show abc123 --body

# Compare before/after
f proxy diff --before "5min" --after "now"
```

### Example: Fixing a Bug

```
You: "Login is broken"

Claude:
> Checking trace-summary.json...
> I see POST /api/auth/login returning 401 for the last 5 requests.
> The error response is: {"error": "Invalid token format"}
>
> Looking at your recent code changes... you modified auth.ts 3 minutes ago.
> The issue is on line 42 - you're passing the token without the "Bearer " prefix.
>
> Here's the fix: [shows diff]
```

---

## Summary: Why This Matters

| Without proxyx | With proxyx |
|----------------|-------------|
| "Something is slow" | "zai provider timed out, cerebras fallback added 4s" |
| "Login is broken" | "POST /api/auth returned 401, token format invalid" |
| "Too many requests" | "UserProfile calls /v1/chat/completions on every mount" |
| Check daemon logs manually | Claude reads trace-summary.json automatically |
| Logs lost on restart | Ring buffer persists, zero-cost |
| No correlation | Trace ID links all services |

**The core insight**: Development is about understanding what your code does at runtime. proxyx captures this automatically and makes it available to AI agents helping you write code.
