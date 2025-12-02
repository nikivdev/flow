# Error Log Format for Automatic Fixes

The flow server streams errors via SSE for automatic fix agents to consume. Structure your error logs with rich context to enable effective automatic fixes.

## Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/logs/ingest` | POST | Ingest single or batch logs |
| `/logs/query` | GET | Query logs with filters |
| `/logs/errors/stream` | GET | SSE stream of new errors |

## Error Log Schema

```typescript
interface ErrorLog {
  project: string      // Project identifier (e.g., "web", "api", "cli")
  content: string      // Error message - be descriptive
  timestamp: number    // Unix timestamp in milliseconds
  type: "error"        // Must be "error" for fix agents
  service: string      // Service/component name (e.g., "auth", "database")
  stack?: string       // Stack trace - critical for automatic fixes
  format: "text" | "json"
}
```

## Best Practices for Automatic Fixes

### 1. Include Full Stack Traces

Stack traces are essential for locating the error source:

```typescript
// Good - includes file, line, column
{
  "content": "TypeError: Cannot read property 'email' of undefined",
  "stack": "TypeError: Cannot read property 'email' of undefined\n    at getUser (/app/src/services/user.ts:42:15)\n    at handleRequest (/app/src/routes/api.ts:18:10)",
  ...
}

// Bad - no stack trace, agent can't locate the error
{
  "content": "TypeError: Cannot read property 'email' of undefined",
  ...
}
```

### 2. Use Absolute File Paths

Prefer absolute paths in stack traces:

```
at getUser (/Users/dev/myapp/src/services/user.ts:42:15)  ✓
at getUser (src/services/user.ts:42:15)                   ✓ (relative ok)
at getUser (user.ts:42:15)                                ✗ (ambiguous)
```

### 3. Descriptive Error Messages

Include context in the error message:

```typescript
// Good
"Failed to parse user response: expected 'email' field but got undefined. Input: {id: 123, name: 'test'}"

// Bad
"undefined error"
```

### 4. Structured JSON Format (Optional)

For complex errors, use `format: "json"` with structured content:

```typescript
{
  "project": "api",
  "content": JSON.stringify({
    "error": "ValidationError",
    "message": "Invalid user data",
    "field": "email",
    "received": null,
    "expected": "string",
    "context": {
      "endpoint": "/api/users",
      "method": "POST",
      "requestId": "abc123"
    }
  }),
  "timestamp": Date.now(),
  "type": "error",
  "service": "validation",
  "stack": "...",
  "format": "json"
}
```

## Error Categories the Fix Agent Handles

| Category | Example | Auto-Fix Capability |
|----------|---------|---------------------|
| `TypeError` | Cannot read property 'x' of undefined | High - adds optional chaining |
| `ReferenceError` | x is not defined | Medium - suggests imports |
| `SyntaxError` | Unexpected token | Low - needs manual review |
| `Import errors` | Cannot find module 'x' | High - suggests npm install |
| `Validation` | Invalid field type | Medium - adds type guards |
| `Connection` | ECONNREFUSED | Low - infrastructure issue |

## Sending Errors from Your App

### TypeScript/JavaScript

```typescript
interface ErrorPayload {
  project: string
  content: string
  timestamp: number
  type: "error"
  service: string
  stack?: string
  format: "text" | "json"
}

async function reportError(error: Error, service: string) {
  const payload: ErrorPayload = {
    project: process.env.PROJECT_NAME || "unknown",
    content: error.message,
    timestamp: Date.now(),
    type: "error",
    service,
    stack: error.stack,
    format: "text"
  }

  await fetch("http://127.0.0.1:9060/logs/ingest", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload)
  })
}

// Global error handler
process.on("uncaughtException", (error) => {
  reportError(error, "process")
})

// Express/Hono middleware
app.use((err, req, res, next) => {
  reportError(err, "http")
  res.status(500).json({ error: "Internal error" })
})
```

### React Error Boundary

```typescript
class ErrorBoundary extends React.Component {
  componentDidCatch(error: Error, info: React.ErrorInfo) {
    fetch("http://127.0.0.1:9060/logs/ingest", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        project: "web",
        content: error.message,
        timestamp: Date.now(),
        type: "error",
        service: "react",
        stack: error.stack + "\n\nComponent Stack:\n" + info.componentStack,
        format: "text"
      })
    })
  }
}
```

## Consuming the Error Stream

Connect to the SSE endpoint to receive errors in real-time:

```typescript
const events = new EventSource("http://127.0.0.1:9060/logs/errors/stream")

events.onmessage = (e) => {
  const error = JSON.parse(e.data)
  console.log(`New error in ${error.project}/${error.service}:`, error.content)

  // Trigger fix agent
  attemptFix(error)
}

events.onerror = (e) => {
  console.error("SSE connection error:", e)
}
```

## Testing

1. Start the flow server:
   ```bash
   f server
   ```

2. Send a test error:
   ```bash
   curl -X POST http://127.0.0.1:9060/logs/ingest \
     -H "Content-Type: application/json" \
     -d '{
       "project": "test",
       "content": "TypeError: Cannot read property '\''foo'\'' of undefined",
       "timestamp": '$(date +%s000)',
       "type": "error",
       "service": "test",
       "stack": "TypeError: Cannot read property '\''foo'\'' of undefined\n    at test (/app/src/index.ts:10:5)",
       "format": "text"
     }'
   ```

3. Watch the stream:
   ```bash
   curl -N http://127.0.0.1:9060/logs/errors/stream
   ```
