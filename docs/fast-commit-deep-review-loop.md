# Fast Commit + Deep Codex Review Loop

This guide configures a speed-first commit workflow with deferred deep review:

- Fast lane now: commit immediately with low-latency model fallbacks (GLM/Cerebras via `zerg/ai`).
- Deep lane later: batch Codex reviews across queued commits.

## 1) Configure `~/code/myflow/flow.toml`

```toml
[commit]
queue = false
queue_on_issues = false

message_fallbacks = [
  "rise:zai:glm-5",
  "rise:cerebras:gpt-oss-120b",
  "remote",
  "openai"
]

review_fallbacks = [
  "glm5",
  "rise:cerebras:gpt-oss-120b",
  "codex-high"
]

[options]
# Optional: mirror commit + queued-review results to myflow.
myflow_mirror = true
# myflow_url = "https://myflow.sh"
# myflow_token = "..."

# Optional: route Codex review through a wrapper transport binary.
# Must implement `app-server` JSON-RPC compatibility.
# codex_bin = "/Users/nikiv/code/flow/scripts/codex-jazz-wrapper"
```

## 2) Daily loop

```bash
# Fast default commit path (quick lane)
f commit

# Later, run deep Codex review across backlog
f reviews-todo codex --all

# Inspect pending/updated deep-review todos
f reviews-todo list

# Approve once issues are addressed
f reviews-todo approve-all
```

## 3) Fixing attached issues without copy/paste

- Each reviewed commit writes a report under `~/.flow/commits/`.
- Use that report directly:

```bash
f fix ~/.flow/commits/<report>.md
```

This replaces manual “copy `f commit` output into Codex and ask to address all issues”.

## Notes

- Plain `f commit` already uses the fast lane (`--quick`) by default. Set `quick-default = false` only if you want blocking review as the default.
- Async queued Codex reviews now emit `commit_queue_review` mirror sync events to myflow/gitedit when the reviewed commit is current `HEAD` (the default `f commit --quick` flow).
- `f reviews-todo codex --all` is a workflow alias over commit queue deep review.
