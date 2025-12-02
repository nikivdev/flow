# Reactive Log Stream Processing

This document describes how to build a TypeScript service that watches the flow logs database for new entries and takes action on errors (e.g., sending macOS notifications).

## Architecture

```
┌─────────────┐     POST      ┌──────────────┐     writes     ┌─────────────┐
│  Your Apps  │ ────────────► │  f server    │ ─────────────► │  flow.db    │
└─────────────┘               └──────────────┘                └─────────────┘
                                                                     │
                                                                     │ watches
                                                                     ▼
                                                              ┌─────────────┐
                                                              │  Log Watcher│
                                                              │  (this doc) │
                                                              └─────────────┘
                                                                     │
                                                                     │ on error
                                                                     ▼
                                                              ┌─────────────┐
                                                              │  Actions    │
                                                              │  - notify   │
                                                              │  - webhook  │
                                                              │  - AI fix   │
                                                              └─────────────┘
```

## Implementation

### Option 1: Polling (Simple)

Poll the database for new logs since the last check.

```typescript
// log-watcher.ts
import Database from "bun:sqlite";
import { exec } from "child_process";
import { homedir } from "os";
import { join } from "path";

const DB_PATH = join(homedir(), ".config/flow/flow.db");
const POLL_INTERVAL_MS = 1000;

interface LogEntry {
  id: number;
  project: string;
  content: string;
  timestamp: number;
  log_type: string;
  service: string;
  stack: string | null;
  format: string;
}

function sendMacNotification(title: string, message: string) {
  const escaped = message.replace(/"/g, '\\"').substring(0, 200);
  exec(
    `osascript -e 'display notification "${escaped}" with title "${title}"'`
  );
}

async function onError(entry: LogEntry) {
  console.log(`[ERROR] ${entry.project}/${entry.service}: ${entry.content}`);

  // Action 1: macOS notification
  sendMacNotification(
    `Error in ${entry.project}`,
    `${entry.service}: ${entry.content}`
  );

  // Action 2: Call AI to analyze/fix (placeholder)
  // await analyzeWithAI(entry);
}

function watchLogs() {
  const db = new Database(DB_PATH, { readonly: true });
  let lastId = 0;

  // Get the current max ID to start from
  const latest = db.query("SELECT MAX(id) as max_id FROM logs").get() as {
    max_id: number | null;
  };
  lastId = latest?.max_id ?? 0;

  console.log(`Watching logs from id > ${lastId}...`);

  setInterval(() => {
    const newLogs = db
      .query(
        `
      SELECT id, project, content, timestamp, log_type, service, stack, format
      FROM logs
      WHERE id > ?
      ORDER BY id ASC
    `
      )
      .all(lastId) as LogEntry[];

    for (const log of newLogs) {
      lastId = log.id;

      if (log.log_type === "error") {
        onError(log);
      }
    }
  }, POLL_INTERVAL_MS);
}

watchLogs();
```

Run with:

```bash
bun log-watcher.ts
```

### Option 2: File System Watch (More Reactive)

Watch the SQLite file for changes using fs notifications.

```typescript
// log-watcher-fs.ts
import Database from "bun:sqlite";
import { watch } from "fs";
import { exec } from "child_process";
import { homedir } from "os";
import { join } from "path";

const DB_PATH = join(homedir(), ".config/flow/flow.db");

interface LogEntry {
  id: number;
  project: string;
  content: string;
  timestamp: number;
  log_type: string;
  service: string;
  stack: string | null;
  format: string;
}

function sendMacNotification(title: string, message: string) {
  const escaped = message.replace(/"/g, '\\"').substring(0, 200);
  exec(
    `osascript -e 'display notification "${escaped}" with title "${title}"'`
  );
}

async function onError(entry: LogEntry) {
  console.log(`[ERROR] ${entry.project}/${entry.service}: ${entry.content}`);
  sendMacNotification(
    `Error in ${entry.project}`,
    `${entry.service}: ${entry.content}`
  );
}

function createWatcher() {
  let lastId = 0;
  let debounceTimer: Timer | null = null;

  function checkNewLogs() {
    const db = new Database(DB_PATH, { readonly: true });

    try {
      if (lastId === 0) {
        const latest = db.query("SELECT MAX(id) as max_id FROM logs").get() as {
          max_id: number | null;
        };
        lastId = latest?.max_id ?? 0;
        console.log(`Starting from id ${lastId}`);
        return;
      }

      const newLogs = db
        .query(
          `
        SELECT id, project, content, timestamp, log_type, service, stack, format
        FROM logs WHERE id > ? ORDER BY id ASC
      `
        )
        .all(lastId) as LogEntry[];

      for (const log of newLogs) {
        lastId = log.id;
        if (log.log_type === "error") {
          onError(log);
        }
      }
    } finally {
      db.close();
    }
  }

  // Initial check
  checkNewLogs();

  // Watch for file changes
  watch(DB_PATH, (eventType) => {
    if (eventType === "change") {
      // Debounce rapid changes
      if (debounceTimer) clearTimeout(debounceTimer);
      debounceTimer = setTimeout(checkNewLogs, 100);
    }
  });

  console.log(`Watching ${DB_PATH} for changes...`);
}

createWatcher();
```

### Option 3: HTTP Streaming Endpoint (Future)

Add a streaming endpoint to `f server` for real-time log delivery via SSE.

```rust
// In log_server.rs (future enhancement)
async fn logs_stream() -> impl IntoResponse {
    // Server-Sent Events stream
    // Clients connect and receive new logs in real-time
}
```

Client would consume:

```typescript
const events = new EventSource("http://127.0.0.1:9060/logs/stream");
events.onmessage = (e) => {
  const log = JSON.parse(e.data);
  if (log.type === "error") {
    handleError(log);
  }
};
```

## Actions on Error

### macOS Notification

```typescript
import { exec } from "child_process";

function notify(title: string, message: string, sound = "default") {
  const escaped = message.replace(/"/g, '\\"').substring(0, 200);
  exec(
    `osascript -e 'display notification "${escaped}" with title "${title}" sound name "${sound}"'`
  );
}
```

### Webhook

```typescript
async function sendWebhook(url: string, entry: LogEntry) {
  await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      text: `Error in ${entry.project}/${entry.service}: ${entry.content}`,
      entry,
    }),
  });
}
```

### AI Analysis

```typescript
async function analyzeWithAI(entry: LogEntry) {
  const response = await fetch("https://api.anthropic.com/v1/messages", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "x-api-key": process.env.ANTHROPIC_API_KEY!,
      "anthropic-version": "2023-06-01",
    },
    body: JSON.stringify({
      model: "claude-sonnet-4-20250514",
      max_tokens: 1024,
      messages: [
        {
          role: "user",
          content: `Analyze this error and suggest a fix:

Project: ${entry.project}
Service: ${entry.service}
Error: ${entry.content}
${entry.stack ? `Stack trace:\n${entry.stack}` : ""}

Provide a brief analysis and actionable fix.`,
        },
      ],
    }),
  });

  const data = await response.json();
  const analysis = data.content[0].text;

  // Send analysis as notification or log it
  console.log("AI Analysis:", analysis);
  notify("AI Fix Suggestion", analysis.substring(0, 200));
}
```

## Full Example: Error Monitor Service

```typescript
// error-monitor.ts
import Database from "bun:sqlite";
import { watch } from "fs";
import { exec } from "child_process";
import { homedir } from "os";
import { join } from "path";

const DB_PATH = join(homedir(), ".config/flow/flow.db");

interface LogEntry {
  id: number;
  project: string;
  content: string;
  timestamp: number;
  log_type: string;
  service: string;
  stack: string | null;
  format: string;
}

interface Config {
  notify: boolean;
  webhook?: string;
  aiAnalysis: boolean;
  projectFilter?: string[];
}

const config: Config = {
  notify: true,
  aiAnalysis: false, // Enable if you have ANTHROPIC_API_KEY set
  // projectFilter: ['my-app'], // Only watch specific projects
};

function notify(title: string, message: string) {
  if (!config.notify) return;
  const escaped = message.replace(/"/g, '\\"').substring(0, 200);
  exec(
    `osascript -e 'display notification "${escaped}" with title "${title}" sound name "Basso"'`
  );
}

async function handleError(entry: LogEntry) {
  // Skip if project filter is set and doesn't match
  if (
    config.projectFilter &&
    !config.projectFilter.includes(entry.project)
  ) {
    return;
  }

  const timestamp = new Date(entry.timestamp).toLocaleTimeString();
  console.log(
    `\n[${timestamp}] ERROR in ${entry.project}/${entry.service}`
  );
  console.log(`  ${entry.content}`);
  if (entry.stack) {
    console.log(`  Stack: ${entry.stack.split("\n")[0]}`);
  }

  // Send notification
  notify(`${entry.project} error`, `${entry.service}: ${entry.content}`);

  // Send to webhook if configured
  if (config.webhook) {
    fetch(config.webhook, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(entry),
    }).catch(console.error);
  }
}

function startWatcher() {
  let lastId = 0;
  let debounceTimer: Timer | null = null;

  function checkNewLogs() {
    const db = new Database(DB_PATH, { readonly: true });

    try {
      if (lastId === 0) {
        const latest = db
          .query("SELECT MAX(id) as max_id FROM logs")
          .get() as { max_id: number | null };
        lastId = latest?.max_id ?? 0;
        return;
      }

      const newLogs = db
        .query(
          `SELECT id, project, content, timestamp, log_type, service, stack, format
           FROM logs WHERE id > ? ORDER BY id ASC`
        )
        .all(lastId) as LogEntry[];

      for (const log of newLogs) {
        lastId = log.id;
        if (log.log_type === "error") {
          handleError(log);
        }
      }
    } finally {
      db.close();
    }
  }

  checkNewLogs();

  watch(DB_PATH, (eventType) => {
    if (eventType === "change") {
      if (debounceTimer) clearTimeout(debounceTimer);
      debounceTimer = setTimeout(checkNewLogs, 50);
    }
  });

  console.log("Error monitor started");
  console.log(`Watching: ${DB_PATH}`);
  console.log(`Notifications: ${config.notify ? "enabled" : "disabled"}`);
  if (config.projectFilter) {
    console.log(`Projects: ${config.projectFilter.join(", ")}`);
  }
  console.log("");
}

startWatcher();
```

Run as a background service:

```bash
# Run in foreground
bun error-monitor.ts

# Run in background
nohup bun error-monitor.ts > /tmp/error-monitor.log 2>&1 &
```

## Testing

1. Start the log server: `f server`
2. Start the watcher: `bun error-monitor.ts`
3. Send a test error:

```bash
curl -X POST http://127.0.0.1:9060/logs/ingest \
  -H "Content-Type: application/json" \
  -d '{"project":"test","content":"Test error","timestamp":'$(date +%s000)',"type":"error","service":"test","format":"text"}'
```

You should see the error logged and receive a macOS notification.
