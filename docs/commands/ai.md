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
f ai codex sessions
f ai codex sessions --path ~/repos/mark3labs/kit
f ai cursor list
f codex resolve "latest"
f codex resolve "https://linear.app/.../project/.../overview" --json
f codex open "continue the deploy work"
f codex agent list
f codex agent show commit
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

### Codex open and resolve

Use `open` when you want one Codex entrypoint that stays conservative about context:

```bash
f codex open
f codex open "continue the deploy work"
f codex open "resume latest"
f codex open --path ~/work/example-project "what was I doing here"
f codex resolve "https://linear.app/fl2024008/project/llm-proxy-v1-6cd0a041bd76/overview" --json
f codex doctor --path ~/work/example-project
```

Behavior:

- no query: start a fresh Codex session in the target repo
- explicit session lookup queries like `latest`, `resume session`, ordinals, or session IDs: resume the matching Codex session
- explicit recovery prompts like `what was I doing` or `continue the ... work`: build a compact recovery handoff and start a new session
- matching reference resolvers: inject only compact resolver output, then append the user request
- otherwise: start a new session with the raw query and no extra wrapper text

This keeps prompt cost flat unless Flow has a strong reason to recover or unroll context.
Use `f codex doctor` to confirm whether wrapper transport, runtime skills, and context budgets are actually active for the current repo.

### Codex sessions after a crash or restart

If your Mac restarts and you lose the live Codex terminals, use:

```bash
f ai codex sessions
f ai codex sessions --path ~/repos/mark3labs/kit
```

Behavior:

- lists recent Codex sessions for the current path
- sorts by the last message timestamp descending
- shows the stable session id plus a preview of the latest message
- the numeric index matches `continue`, so you can reopen quickly

Examples:

```bash
f ai codex continue 1 --path ~/repos/mark3labs/kit
f ai codex continue 019cd046 --path ~/repos/mark3labs/kit
f ai codex sessions --path ~/repos/mark3labs/kit --json
```

### Optional `flow.toml` resolver config

You can teach `f codex open` and `f codex resolve` to unroll repo-specific references:

```toml
[codex]
auto_resolve_references = true
prompt_context_budget_chars = 900
max_resolved_references = 1
sync_workflow_command = "./scripts/sync-safe"

[[codex.reference_resolver]]
name = "linear"
match = ["https://linear.app/*/issue/*", "https://linear.app/*/project/*"]
command = "my-linear-tool inspect {{ref}} --json"
inject_as = "linear"
```

Notes:

- configure this in repo `flow.toml` or global `~/.config/flow/flow.toml`
- `{{ref}}`, `{{query}}`, and `{{cwd}}` are available in resolver commands
- built-in Linear URL parsing works even without a custom resolver
- resolver output is compacted before prompt injection
- `prompt_context_budget_chars` hard-caps injected context before your request is appended
- `max_resolved_references` prevents broad unrolling from bloating one turn
- `sync_workflow_command` lets plain `sync branch` route into a repo-specific safe sync command instead of improvised Git/JJ steps

### Optional runtime skill transport

Flow can also materialize tiny per-launch runtime skills for current upstream Codex without forking Codex.

Enable it globally with:

```bash
f codex enable-global --full
f codex doctor --path ~/docs --assert-runtime --assert-schedule
```

Or configure it manually with:

```toml
[codex]
runtime_skills = true

[options]
codex_bin = "~/code/flow/scripts/codex-flow-wrapper"
```

For the `~/code/flow` repo specifically, the intended in-repo shape is:

```toml
[options]
codex_bin = "~/code/flow/scripts/codex-flow-wrapper"

[codex]
runtime_skills = true
auto_resolve_references = true
prompt_context_budget_chars = 1200
max_resolved_references = 2

[[codex.skill_source]]
name = "run-control-plane"
path = "~/run"
enabled = true
```

That keeps a fresh checkout Codex-first even before machine-local setup is remembered.

Current first-slice behavior:

- `f codex open "write plan"` can attach a tiny plan-writing runtime skill
- the runtime skill is exposed only for the launched Codex process
- Flow keeps the generated runtime state under `~/.config/flow/codex/runtime`

Inspect or clear runtime state:

```bash
f codex runtime show
f codex runtime clear
f codex memory status
f codex memory query --path ~/code/flow "codex control plane runtime skills"
f codex memory recent --path ~/docs
f codex doctor
```

Assertive health checks:

```bash
f codex doctor --path ~/docs --assert-runtime
f codex doctor --path ~/docs --assert-schedule
f codex doctor --path ~/docs --assert-learning
f codex doctor --path ~/docs --assert-autonomous
```

`--assert-learning` is intentionally strict: it fails until Flow has real
logged events, grounded outcome samples, and a non-empty scorecard for that
target.

Built-in plan writer:

```bash
cat <<'EOF' | f codex runtime write-plan --title "Example Plan"
# Example Plan

- item
EOF
```

By default this writes to today's `~/plan/<day-of-month>/` bucket, for example
`~/plan/23/` on the 23rd day of the month.

### Run-owned agent bridge

Flow can execute run-owned Codex agents directly without copying their specs or
prompts into `~/code/flow`.

Useful commands:

```bash
f codex agent list
f codex agent list --json
f codex agent show commit
f codex agent run planner --path ~/code/flow "make a 3 phase rollout plan"
f codex agent run commit --path ~/repos/openai/codex "review the current diff and draft a safe commit"
f codex agent run planner --json --path ~/code/flow "reply with exactly: ok"
```

Behavior:

- Flow bridges into `~/run/scripts/agent-router.sh`
- the run-owned agent still executes through its own spec/runtime path in `~/run`
- `--path` controls the repo/worktree cwd used for the agent run
- `run` returns the agent's final output plus any durable artifact/trace paths
- `f codex doctor --path <repo>` reports whether the run-agent bridge is ready

Repo-local readiness check:

```bash
f codex doctor --path ~/code/flow --assert-runtime --assert-schedule
f codex skill-source list --path ~/code/flow
f codex agent list
```

### Skill eval and background refresh

Flow can learn which runtime skills are actually worth injecting from local
Codex usage history without replaying Codex in the hot path.

Useful commands:

```bash
f codex eval --path ~/work/example-project
f codex memory sync --limit 400
f codex memory recent --path ~/work/example-project --limit 12
f codex skill-eval show --path ~/work/example-project
f codex skill-eval run --path ~/work/example-project
f codex skill-eval cron --limit 400 --max-targets 12 --within-hours 168
f codex telemetry status
f codex telemetry flush --limit 200
f codex trace status
f codex trace current-session --json
f codex trace inspect <trace-id> --json
f codex project-ai show --path ~/work/example-project --json
f codex project-ai recent --limit 12 --json
f codex skill-source list --path ~/work/example-project
f codex skill-source sync --path ~/work/example-project --skill find-skills
f codex agent list
f codex agent run planner --path ~/work/example-project "summarize the next rollout slice"
```

The Codex memory mirror:

- stores durable indexed memory under the Jazz2 root (`~/.jazz2/...` or `~/repos/garden-co/jazz2/.jazz2/...`)
- mirrors Flow’s route/outcome history into SQLite with WAL enabled
- extracts compact repo/code facts from repo capsules (summary, commands, important paths, docs hints)
- adds bounded live code-path retrieval for explicit repo references, so prompts like `see ~/code/flow ...` can inject likely files such as `src/ai.rs` or `docs/...` without dumping raw source
- indexes durable repo entrypoints and extracted symbols under the same Jazz2-rooted memory store, then supplements them with live symbol extraction from the top-ranked code files during `memory query` / `codex resolve`
- adds tiny symbol snippets for the top code hits, so coding prompts can carry actual struct/function shape without inlining whole files
- biases retrieval by intent: implementation/file-edit prompts prefer symbols, snippets, and `src/...` paths; summary/docs prompts prefer doc headings and docs paths
- stays best-effort so failed memory writes do not block normal Codex coding turns

The project-ai manifest:

- gives `codexd` a bounded metadata view of repo-local `.ai/` scaffolding
- records counts and latest paths for skills, docs, reviews, tasks, and todos
- keeps default context fill minimal by exposing only compact hints, not raw content
- tracks query counts and recent repo usage so you can see whether the feature is actually being used
- is refreshed again by `f codex skill-eval cron`, so the mirror heals even if a hot-path write is skipped
- is queried automatically for explicit repo references during `f codex open` / `f codex resolve`

What `cron` does:

- scans only recent logged Flow Codex events
- syncs recent skill-eval logs into the Jazz2-backed memory mirror
- skips missing/moved repo paths
- rebuilds scorecards for a bounded number of recent repos
- never launches Codex or replays network work in the background

For your use case, this keeps learning cheap and safe enough to run regularly.

### Trace inspection for Flow-managed Codex sessions

Flow now assigns a trace envelope to all Flow-managed Codex launches, not just
special workflows like `check <github-pr-url>`.

That means sessions started or resumed through:

```bash
j ...
k <session-id>
f codex open ...
f codex resume ...
f codex continue ...
```

carry `FLOW_TRACE_*` env vars and emit at least one compact Flow telemetry
event, so the session becomes remotely inspectable.

Useful commands:

```bash
f codex trace status
f codex trace current-session --json
f codex trace inspect <trace-id> --json
```

Behavior:

- `trace status` checks whether Maple MCP reads are configured and reachable
- `trace current-session` reads the active `FLOW_TRACE_ID` from the current
  Flow-managed Codex session, flushes recent Flow telemetry once, then attempts
  a remote trace read
- if Maple reads are partially configured but tenant access is still blocked,
  Flow returns the current trace metadata plus `readError` instead of failing
  without context

This keeps the workflow autonomous from inside Codex itself: once the session
was started through Flow, you can inspect the current trace without manually
copying ids.

`f codex eval --path ...` is the joined operator report:

- current runtime/doctor state
- recent route mix and context cost
- grounded skill scorecard highlights
- concrete “what to improve next” recommendations
- commands to deepen or fix the current state

If `codexd` is running, it also keeps recent completion reconciliation warm and
now does a bounded background scorecard refresh, so the eval report does not go
stale as quickly between manual runs.

Optional telemetry export follows the same local-first pattern: Flow keeps
local Codex logs canonical, and if `FLOW_CODEX_MAPLE_*` env vars are set it can
export redacted route/context/outcome spans to Maple without shipping raw
prompts or full repo paths. Use:

```bash
f codex telemetry status
f codex telemetry flush --limit 200
```

For local-only secrets, prefer `f env set --personal ...`. On macOS you may
need `f env unlock` once per day for background reads. Flow-launched Codex
sessions also inherit the same `FLOW_CODEX_MAPLE_*` values from the personal
store, while explicit shell env still wins for one-off tests.

When `codexd` is running, the daemon also performs a bounded background export
pass so the external analytics view stays warm.

### macOS launchd schedule for skill-eval

If you want scorecards to stay fresh automatically on macOS:

```bash
f codex-skill-eval-launchd-install
f codex-skill-eval-launchd-status
f codex-skill-eval-launchd-logs
```

`f codex enable-global --full` installs this schedule for you.

Default schedule:

- every 30 minutes
- scan up to 400 recent events
- rebuild up to 12 recent repo scorecards
- ignore repos not seen in the last 168 hours

You can tune install-time bounds:

```bash
f codex-skill-eval-launchd-install --minutes 20 --limit 600 --max-targets 16 --within-hours 72
f codex-skill-eval-launchd-install --dry-run
```

Remove it with:

```bash
f codex-skill-eval-launchd-uninstall
```

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
