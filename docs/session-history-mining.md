# Session History Mining for Claude/Codex

Use this when you want an AI agent to study recent Claude/Codex work before proposing a plan.

This is optimized for:
- cross-project history review
- low-noise context transfer
- token efficiency (only new or condensed context)

## What to Use

Use Flow's cross-project session browser:

```bash
f sessions
```

`f sessions` scans Claude + Codex sessions across projects, lets you pick one, and copies context to clipboard.

## Core Commands

```bash
# List sessions across all projects without interactive selection
f sessions --list

# Only Claude sessions
f sessions --provider claude --list

# Only Codex sessions
f sessions --provider codex --list

# Copy selected session context (interactive picker via fzf)
f sessions --provider all

# Copy only the last N exchanges
f sessions --provider all --count 8

# Ignore checkpoints and copy full session
f sessions --provider all --full

# Produce a condensed handoff summary (requires Gemini key)
f sessions --provider all --handoff
```

## Checkpoint Behavior (Important)

Default `f sessions` copies context since last consumption checkpoint for the current repo.

That means repeated runs do not keep re-copying old context.

Checkpoint file:

```text
.ai/internal/consumed-checkpoints.json
```

Use `--full` when you explicitly want full history instead of incremental context.

## Current-Repo Deep Pull (When Needed)

If you need more detail from a known session in the current repo:

```bash
f ai claude list
f ai codex list

# Copy the last 6 exchanges from a selected Claude session for this repo
f ai claude context - /absolute/path/to/repo 6
```

Use this after `f sessions` when you want to zoom in on one thread.

## Efficient Workflow (Recommended)

1. In the target repo where you want the plan, run:
   `f sessions --provider all --list`
2. Pull 2 to 4 high-signal contexts:
   `f sessions --provider claude --count 6`
   `f sessions --provider codex --count 6`
3. For stale/long sessions, prefer condensed transfer:
   `f sessions --provider all --handoff`
4. Paste each copied output into labeled blocks in your prompt.
5. Ask for a plan with explicit constraints and ranked execution order.

## Prompt Scaffold (Attach This)

Use this format when asking an agent to mine history and propose execution:

```text
I have ~$500 of Claude tokens expiring in <N> day(s) and want to use them efficiently.

Goal:
- study goose and propose a concrete execution plan for token usage
- use ideas from recent Claude/Codex histories
- rank ideas by expected impact and execution cost

Constraints:
- avoid low-signal exploration
- maximize useful output per token
- include exact next commands I should run

Session context 1:
<paste from f sessions --provider claude --count 6>

Session context 2:
<paste from f sessions --provider codex --count 6>

Session context 3 (optional handoff):
<paste from f sessions --provider all --handoff>

Deliver:
1. top opportunities (ranked)
2. 48-hour execution plan
3. fallback plan if one assumption fails
4. specific commands and owners
```

## Token-Efficiency Rules

- Prefer `--count` over `--full` unless you are reconstructing full intent.
- Prefer `--handoff` for large stale sessions before pasting into expensive models.
- Merge duplicate context manually before sending to avoid repeated tokens.
- Request ranked outputs with hard deliverables (plan, commands, owners, fallback).

## Troubleshooting

- `fzf not found`: install `fzf`, or use `--list` and then run interactive once `fzf` is available.
- No new context copied: expected if checkpoint is current; rerun with `--full`.
- `--handoff` not working: set `GEMINI_API_KEY` or `GOOGLE_API_KEY`.
