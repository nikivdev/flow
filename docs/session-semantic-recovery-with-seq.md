# Semantic Session Recovery with Seq (Claude/Codex)

Use this when local Claude/Codex session state got wiped (for example after a machine reset), and you want to recover work by searching prior sessions semantically.

This workflow uses:
- Flow session commands (`f ai ...`) for exact resume behavior
- Seq's zvec-backed session index for semantic retrieval + fuzzy picker

## What You Get

- Fast semantic search over historical Claude/Codex Q/A pairs
- Scope to the current repo path
- Picker flow similar to Flow fuzzy task flows (`fzf`)
- Direct resume command output:
  - `f ai claude resume <session-id>`
  - `f ai codex resume <session-id>`

## Prerequisites

1. `seq` repo exists at `~/code/seq`.
2. Agent Q/A capture has data in `~/repos/alibaba/zvec/data/agent_qa.jsonl`.
3. `fzf` installed for interactive picker.

## One-Time Setup

From `~/code/seq`:

```bash
f rl-capture-on
f agent-qa-capture-on
```

If you need historical backfill:

```bash
f agent-qa-capture-on-backfill
```

Quick status check:

```bash
f agent-qa-capture-status
```

## Primary Commands

From `~/code/seq`:

```bash
# Semantic search + interactive picker + auto-resume
f agent-session-search "router regression around branch sync"

# Open picker with no query (recent-first)
f agent-session-search

# Non-interactive listing
f agent-session-search-list "skill sync force reload"
```

Provider filter:

```bash
f agent-session-search --provider claude "deploy rollback"
f agent-session-search --provider codex "trace parser failure"
```

## Use from Any Repo

If you are in another repo and want path-attached session search without changing directory:

```bash
f run --config ~/code/seq/flow.toml agent-session-search --path "$(pwd)" "your query"
```

List-only variant:

```bash
f run --config ~/code/seq/flow.toml agent-session-search-list --path "$(pwd)" "your query"
```

## Recovery Playbook After Reset

1. Go to target repo:
   - `cd /path/to/repo`
2. Run semantic search via Seq task:
   - `f run --config ~/code/seq/flow.toml agent-session-search --path "$(pwd)" "<query>"`
3. Pick best session in `fzf`.
4. Flow resumes exact session ID with strict provider behavior.
5. If needed, inspect normal repo-local session list:
   - `f ai claude list`
   - `f ai codex list`

## Troubleshooting

- Repo path changed (rename/move):
  - Run `f code move-sessions --from /old/path --to /new/path`.
  - This migrates Claude/Codex session paths and Seq zvec `agent_qa.jsonl` metadata so folder-scoped semantic search still matches the new path.
- No results:
  - Run `f agent-qa-capture-once --backfill --reset-state` in `~/code/seq`.
- No picker:
  - Install `fzf`, or use `agent-session-search-list`.
- Wrong scope:
  - Pass explicit `--path /absolute/repo/path`.
