# f ai

Manage Claude Code, Codex, and Cursor sessions for the current project.

Flow reads local session stores, filters by current project path, and gives you one interface for list/search/resume/copy/save.
When you need to reopen a repo's session from another working directory, use `--path <project-root>` on `resume` or provider-specific `continue`.
Cursor transcripts are supported for reading only.

## Quick Start

```bash
# Fuzzy-pick a recent session (Claude + Codex + Cursor)
f ai

# Provider-specific list/read
f ai claude list
f ai codex list
f ai cursor list
f ai claude resume <session-id-or-name>
f ai codex resume <session-id-or-name>
f ai codex resume --path ~/work/example-project
f ai codex continue --path ~/work/example-project
f ai cursor context - /path/to/repo 3
f cursor copy

# Save a memorable alias for a session
f ai save reclaim-fix --id a38cf8bf-f4e2-4308-8b27-0254f89c4385
```

## Session Sources

- Claude: `~/.claude/projects/<project-path>/*.jsonl`
- Codex: `~/.codex/sessions/**/*.jsonl` (Flow matches by `session_meta.cwd`)
- Cursor: `~/.cursor/projects/<project-key>/agent-transcripts/<session-id>/<session-id>.jsonl`
- Saved aliases: `.ai/sessions/claude/index.json` in your repo

## Resume Behavior (Important)

### TTY requirement

Resume is interactive by design.

- `f ai claude resume ...` requires a terminal TTY.
- `f ai codex resume ...` requires a terminal TTY.
- In non-interactive shells, Flow exits with a clear error and non-zero status.
- Cursor does not currently expose a Flow resume/continue path; use `list`, `copy`, or `context`.

### Claude exact-ID behavior

If you pass an explicit session (`name`, `id`, or `id-prefix`), Flow is strict:

- it attempts `claude --resume <id>`
- if Claude cannot open that exact session, Flow fails
- Flow does **not** auto-fallback to `--continue` for explicit sessions (prevents opening the wrong conversation)

### Claude no-arg behavior

For `f ai claude resume` with no argument:

- Flow picks most recent Claude session for this project
- if that resume fails, Flow may fallback to `claude --continue` in the same cwd (TTY only)

### Codex behavior

For Codex, Flow runs:

```bash
codex resume <id> --dangerously-bypass-approvals-and-sandbox
```

No fallback is applied on resume failure; Flow returns non-zero.

### Cursor behavior

Cursor transcripts are read-only in Flow:

- `f ai cursor list` opens a picker and copies the selected transcript
- `f ai cursor copy` copies the latest Cursor transcript for this repo
- `f ai cursor context ...` copies the last N exchanges
- `f cursor ...` is a shortcut for the same provider-specific read commands

### Cross-directory resume

You can target another repo without changing directory:

```bash
f ai codex resume --path ~/work/example-project
f ai codex resume --path ~/work/example-project 019c61c5-0aef-71a1-b058-5c9ab43013d4
f ai codex continue --path ~/work/example-project
```

- `resume --path <repo>` resolves the requested session against that repo instead of the current cwd
- `continue --path <repo>` resumes the latest session for that repo
- explicit full Codex IDs still work directly even when your current cwd is different

## Session Selectors

`resume` accepts:

- saved alias from `.ai/sessions/claude/index.json`
- full session ID
- ID prefix (8+ chars)
- numeric index from list output (1-based)

Examples:

```bash
f ai resume my-feature
f ai resume a38cf8bf
f ai claude resume 2
f ai codex resume 019c61c5-0aef-71a1-b058-5c9ab43013d4
f ai cursor context 382ef1a3 /path/to/repo 2
```

## Content Copy Commands

```bash
# Copy full conversation to clipboard
f ai copy
f ai copy <session>

# Copy last N prompt/response turns
f ai context
f ai context <session> <path> <count>
```

Use `-` as session placeholder to trigger fuzzy selection:

```bash
f ai claude context - /path/to/repo 3
f ai cursor context - /path/to/repo 3
```

## Project Workflow (Recommended)

1. Start from repo root and inspect tasks:
   `f tasks list`
2. Resume exact session when continuing prior work:
   `f ai claude resume <id>` or `f ai codex resume <id>`
3. Keep context current:
   `f skills sync` then `f skills reload`
4. Validate through tasks:
   `f test-related` / `f test`
5. Commit through Flow gates:
   `f commit`

This keeps sessions, tasks, skills, and commit quality checks in one loop.

## Everruns Bridge Mode

Flow also supports running a prompt through Everruns while routing client-side
`seq_*` tool calls to local `seqd`:

```bash
f ai everruns "open Safari and take a screenshot"
```

Key points:

- This path is additive. It does not replace `f ai claude ...` or `f ai codex ...`.
- Flow now reuses Seq's canonical Everruns bridge for:
  - `seq_*` tool definitions injected into new sessions
  - tool-name normalization (`seq_open_app`, `seq.open_app`, `seq:open-app`)
  - request correlation IDs (`request_id`, `run_id`, `tool_call_id`)
- Event transport is SSE-first (`/sse`) with automatic fallback to polling (`/events`) if SSE is unavailable.
- Optional Maple telemetry export can dual-write runtime traces to local + hosted ingest endpoints when `SEQ_EVERRUNS_MAPLE_*` env vars are configured.
- Existing Flow features remain unchanged (`f seq-rpc`, session resume/copy/context flows).

Setup and validation details are documented in:

- `docs/everruns-seq-bridge-integration.md`
- `docs/everruns-maple-runbook.md`
