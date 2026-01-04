# web

Open the Flow web UI for the current project.

## Usage

```bash
f web
```

## What it does

- Serves the `.ai/web` UI for the current project.
- Exposes `/api/projects` for project metadata and top-level `.ai` entries.
- Exposes `/api/ai` for the full `.ai` tree (paths + kinds).
- Exposes `/api/sessions` for Claude/Codex session summaries and transcripts.
- Exposes `/api/openapi` when an OpenAPI spec is detected.

## Options

```bash
--host <host>   Host to bind (default: 127.0.0.1)
--port <port>   Port to serve the UI on (default: 9310)
```

## Notes

- `f web` serves `.ai/web/dist` when it exists (Vite default output).
- The Vite app source lives in `.ai/web`.
- `.ai/web` is gitignored by default so AI can freely rewrite it.
- If no build exists, `f web` shows a minimal placeholder.
- Example build: `vite build` from `.ai/web`.
- `f web` now runs `bun install` (once) and `bun run build` automatically when a Vite app is present.
