# AI Traces (Flow)

Use this doc when collecting context for debugging or development work in `~/code/flow`.
The goal is to keep trace collection low overhead while still capturing enough signal for fast fixes.

## Quick start

- Proxy traces summary:
  - `~/.config/flow/proxy/trace-summary.json`
- Last trace event (CLI):
  - `f proxy last`
- Stream traces (CLI):
  - `f proxy trace`

## Trace sources

- **Proxy ring buffer summary**
  - Path: `~/.config/flow/proxy/trace-summary.json`
- **Fish shell IO traces**
  - Path: `${XDG_DATA_HOME:-~/.local/share}/fish/io-trace/last.meta`
  - Command: `fish-trace last`

## When to use which

- Start with the summary file to find errors/slow requests.
- Use `f proxy last` when you need the newest request details.
- Use `f proxy trace` to tail a stream during repro.

## Safety notes

- Summary parsing is cheap and safe to run anytime.
- If proxy is not running, `f proxy` commands will fail; fall back to the summary file and fish traces.

## Skills + harness loading

Codex skills are discovered from `~/.codex/skills` and injected by the AI harness.
If a new skill was added or edited, restart the agent or re-list skills to refresh.
This repo ships trace guidance via the `flow-dev-traces` skill.
