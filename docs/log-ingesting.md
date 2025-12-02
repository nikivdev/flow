# Log Ingestion

Flow includes a log ingestion system for collecting and querying structured logs from your projects. Logs are stored in SQLite for later analysis.

## Starting the Server

```bash
f server
```

This starts the HTTP server on `127.0.0.1:9060` (default). Options:

- `--host <IP>` - Bind address (default: 127.0.0.1)
- `--port <PORT>` - Port number (default: 9060)

## Endpoints

### Health Check

```
GET /health
```

Returns `{"status": "ok"}` when the server is running.

### Ingest Logs

```
POST /logs/ingest
Content-Type: application/json
```

**Single log:**

```json
{
  "project": "my-app",
  "content": "TypeError: Cannot read property 'x' of undefined",
  "timestamp": 1733150000000,
  "type": "error",
  "service": "api",
  "stack": "at handler (api.ts:42)\nat processRequest (server.ts:100)",
  "format": "text"
}
```

**Batch:**

```json
[
  {
    "project": "my-app",
    "content": "Request received",
    "timestamp": 1733150000000,
    "type": "log",
    "service": "api",
    "format": "text"
  },
  {
    "project": "my-app",
    "content": "Database query",
    "timestamp": 1733150001000,
    "type": "log",
    "service": "db",
    "format": "text"
  }
]
```

**Response:**

```json
{ "inserted": 1, "ids": [42] }
```

### Query Logs

```
GET /logs/query
```

**Query parameters:**

| Parameter | Description                            |
| --------- | -------------------------------------- |
| `project` | Filter by project name                 |
| `service` | Filter by service                      |
| `type`    | Filter by log type (`log` or `error`)  |
| `since`   | Timestamp (ms) - logs after this time  |
| `until`   | Timestamp (ms) - logs before this time |
| `limit`   | Max results (default: 100)             |
| `offset`  | Skip N results for pagination          |

**Examples:**

```bash
# All logs
curl "http://127.0.0.1:9060/logs/query"

# Errors for a project
curl "http://127.0.0.1:9060/logs/query?project=my-app&type=error"

# Logs from the last hour
curl "http://127.0.0.1:9060/logs/query?since=$(($(date +%s) * 1000 - 3600000))"
```

## Log Entry Schema

| Field       | Type    | Required | Description                             |
| ----------- | ------- | -------- | --------------------------------------- |
| `project`   | string  | yes      | Project identifier                      |
| `content`   | string  | yes      | Log message or error text               |
| `timestamp` | integer | yes      | Unix timestamp in milliseconds          |
| `type`      | string  | yes      | `"log"` or `"error"`                    |
| `service`   | string  | yes      | Service/task name that produced the log |
| `stack`     | string  | no       | Stack trace for errors                  |
| `format`    | string  | no       | `"text"` (default) or `"json"`          |

## Database

Logs are stored in `~/.config/flow/flow.db` in the `logs` table. You can query directly:

```bash
sqlite3 ~/.config/flow/flow.db "SELECT * FROM logs WHERE log_type='error' ORDER BY timestamp DESC LIMIT 10;"
```

## Client Examples

### TypeScript/JavaScript

```typescript
async function sendLog(entry: {
  project: string;
  content: string;
  type: "log" | "error";
  service: string;
  stack?: string;
}) {
  await fetch("http://127.0.0.1:9060/logs/ingest", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      ...entry,
      timestamp: Date.now(),
      format: "text",
    }),
  });
}

// Usage
sendLog({
  project: "my-app",
  content: "User login failed",
  type: "error",
  service: "auth",
});
```

### Python

```python
import requests
import time

def send_log(project, content, log_type, service, stack=None):
    requests.post("http://127.0.0.1:9060/logs/ingest", json={
        "project": project,
        "content": content,
        "timestamp": int(time.time() * 1000),
        "type": log_type,
        "service": service,
        "stack": stack,
        "format": "text"
    })

# Usage
send_log("my-app", "Database connection failed", "error", "db")
```

### curl

```bash
curl -X POST http://127.0.0.1:9060/logs/ingest \
  -H "Content-Type: application/json" \
  -d '{"project":"my-app","content":"Test error","timestamp":'$(date +%s000)',"type":"error","service":"cli","format":"text"}'
```

## Testing

Run the test task to verify the system is working:

```bash
# Terminal 1: Start the server
f server

# Terminal 2: Run tests
f test-log-server
```
