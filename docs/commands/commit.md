# f commit

AI-powered commit with deferred Codex review and GitEdit sync.

## Overview

Stages all changes, commits quickly by default, runs Codex deep review asynchronously in the background, pushes, and syncs AI sessions to gitedit.dev.

## Quick Start

```bash
# Default flow: commit now, Codex review runs in background
f commit

# Blocking pre-commit review (legacy behavior)
f commit --slow

# Fast commit with custom message (no AI review, no Codex follow-up)
f commit --fast "fix typo"
f commit --fast  # defaults to "." as message

# Queue for review (no push) + create jj review bookmark
f commit --queue

# Commit without pushing
f commit -n

# Include AI context in review
f commit --context

# Custom message
f commit -m "Fixes #123"
```

## Options

| Option | Short | Description |
|--------|-------|-------------|
| `--no-push` | `-n` | Skip pushing after commit |
| `--queue` | | Queue the commit for review (no push) |
| `--no-queue` | | Bypass queue and allow push |
| `--sync` | | Run synchronously (don't delegate to hub) |
| `--context` | | Include AI session context in code review |
| `--dry` | | Dry run: show context without committing |
| `--quick` | | Explicitly use fast commit + async Codex review (compat alias) |
| `--slow` | | Run blocking pre-commit review before commit |
| `--fast [MSG]` | | Fast commit with no AI review (defaults to ".") |
| `--codex` | | Use Codex instead of Claude for review |
| `--review-model <MODEL>` | | Choose specific review model |
| `--message <MSG>` | `-m` | Custom message appended to commit |
| `--tokens <N>` | `-t` | Max tokens for AI context (default: 1000) |

## Review Models

| Model | Description |
|-------|-------------|
| `claude-opus` | Claude Opus 1 for review (default) |
| `codex-high` | Codex high-capacity (gpt-5.1-codex-max) |
| `codex-mini` | Codex mini (gpt-5.1-codex-mini) |

```bash
# Use Codex
f commit --codex

# Specific model
f commit --review-model codex-high
```

---

## Fast Commit + Deferred Codex Review

`f commit` now uses this mode by default.

### Default (`f commit`) / `--quick` — commit now, Codex reviews later

Commits immediately (generates an AI commit message but skips the blocking code review), then spawns a background Codex review for that commit. The review result is queued and visible via `f commit-queue list`.

```bash
f commit
f commit --quick
```

What happens:
1. Stages all changes, generates commit message, commits, pushes
2. Queues the commit SHA for async review
3. Spawns a background Codex process that reviews the diff
4. You keep working — check results later with `f commit-queue list`

### `--slow` — run blocking review before commit

Runs the pre-commit AI review before creating the commit.

```bash
f commit --slow
f commit --slow --review-model codex-high
```

### `--fast` — instant commit, no review at all

Commits with a provided message (or `"."` if omitted). No AI review, no async follow-up. Useful for trivial changes.

```bash
f commit --fast "fix typo"
f commit --fast              # message defaults to "."
```

### When to use which

| Flag | AI message | Code review | Async review | Best for |
|------|-----------|-------------|-------------|----------|
| (none) | Yes | No | Yes (Codex background) | Default fast workflow |
| `--quick` | Yes | No | Yes (Codex background) | Explicit fast mode |
| `--slow` | Yes | Yes (blocking) | No | Pre-commit deep check |
| `--fast` | No (you provide) | No | No | Trivial/WIP commits |

### Opt Out Of Fast Default

If you want plain `f commit` to run blocking review by default, set:

```toml
[commit]
quick-default = false
```

With this set, plain `f commit` behaves like `f commit --slow` unless you explicitly pass `--quick`.

Run deep review in batches:

```bash
f reviews-todo codex --all
f reviews-todo list
```

### Myflow Fast + Deep Profile

For `~/code/myflow`, this profile gives a fast default loop while keeping deep Codex coverage:

```toml
[commit]
quick-default = true
queue = false
queue_on_issues = false

# Fast message generation path via zerg/ai (glm + cerebras), then fallbacks.
message_fallbacks = [
  "rise:zai:glm-5",
  "rise:cerebras:gpt-oss-120b",
  "remote",
  "openai"
]

# When you explicitly run blocking review, prefer fast models first.
review_fallbacks = [
  "glm5",
  "rise:cerebras:gpt-oss-120b",
  "codex-high"
]
```

Then operate with:

```bash
f commit
f reviews-todo codex --all
```

---

## What Happens

1. **Safety Checks**
   - Warns about sensitive files (.env, .pem, keys, credentials)
   - Warns about files with large diffs (500+ lines)
   - Runs invariant checks from `[invariants]` when configured

2. **Stage Changes**
   - Runs `git add -A` to stage all changes

3. **Code Review**
   - Sends diff to AI for review
   - Checks for bugs, security issues, best practices
   - Optionally includes AI session context (`--context`)

4. **Generate Message**
   - AI generates commit message from diff
   - Appends custom message if provided (`-m`)

5. **Commit**
   - Creates commit with generated message

6. **Queue (optional)**
   - Adds commit to the review queue (`f commit-queue list`)
   - Creates a jj review bookmark (e.g., `review/main-<sha>`)

7. **Push**
   - Pushes to remote (unless `--no-push` or `--queue`)

7. **GitEdit Sync**
   - Syncs AI session data to gitedit.dev

## Invariant Gate

If your project defines `[invariants]` in `flow.toml`, `f commit` evaluates the staged diff against those rules (forbidden patterns, dependency allowlist policy, file line limits).

- `mode = "warn"`: commit continues and findings are shown.
- `mode = "block"`: commit is blocked on warning/critical findings.
- Findings are injected into the AI review context.

---

## Usage Examples

### Basic Commit

```bash
# Review, commit, and push
f commit
```

### Local Commit Only

```bash
# Don't push to remote
f commit -n
```

### With AI Context

Include recent AI session context in the review:

```bash
f commit --context
f commit --context --tokens 2000  # More context
```

### Custom Message

Append additional context to the commit:

```bash
f commit -m "Fixes #123"
f commit -m "Co-authored-by: John <john@example.com>"
```

### Dry Run

See what would be reviewed without committing:

```bash
f commit --dry
```

### Synchronous Mode

Run directly without delegating to hub:

```bash
f commit --sync
```

---

## Safety Warnings

### Sensitive Files

Before committing, flow warns if staging files that look sensitive:

```
⚠ Warning: The following sensitive files are staged:
  - .env
  - credentials.json
  - private.key
```

Sensitive patterns include:
- `.env`, `.env.*`
- `credentials.json`, `secrets.json`
- `.pem`, `.key`, `id_rsa`, `id_ed25519`
- Files containing `password`, `secret`, `token`

### Secret Scan (Staged Diff)

Flow scans staged diffs for likely secrets (API keys, tokens, passwords). If a match
is detected, the commit is blocked. In an interactive terminal, flow offers to run
an auto-fix using `ai` to mask or replace the values, then asks for approval to
continue.

You can bypass the check for a single commit with:

```
FLOW_ALLOW_SECRET_COMMIT=1 f commit
```

### Large Diffs

Warns about files with significant changes:

```
⚠ Warning: The following files have large diffs:
  - src/generated.rs (1523 lines)
  - data/fixtures.json (834 lines)
```

Threshold: 500+ lines added/removed.

---

## Related Commands

| Command | Description |
|---------|-------------|
| `f commit` | Fast commit + deferred Codex review (default) |
| `f commitWithCheck` | Review without GitEdit sync |
| `f commitSimple` | No review, just AI commit message |

### commitWithCheck (alias: cc)

Same as `commit` but skips GitEdit sync:

```bash
f commitWithCheck
f cc  # Short alias
```

### commitSimple

Quick commit without code review:

```bash
f commitSimple
```

---

## Configuration

### Hub Delegation

By default, `f commit` delegates to the hub daemon for async processing. Use `--sync` for direct execution:

```bash
# Async via hub (default)
f commit

# Sync (direct)
f commit --sync
```

### Commit Message Tool

Optionally force a specific commit-message generator:

```toml
[commit]
message_tool = "kimi" # also supports: claude, rise, glm5, opencode, openrouter, remote, openai
message_model = "kimi-k2-thinking-turbo" # optional, tool-specific
```

If the forced tool fails, `f commit` now falls back through the configured/default chain.

### Review Tool (Kimi CLI)

Use Kimi Code CLI for code review:

```toml
[review]
tool = "kimi"
model = "kimi-k2-thinking-turbo" # optional; uses Kimi default if omitted
```

This uses `kimi --quiet` (print mode) with your existing Kimi CLI auth/config.

### Robust Fallbacks

`f commit` now uses multi-attempt fallback chains for both review and commit-message generation.

Default behavior:
- Review: primary selection, then `openrouter`, `claude`, `codex-high`
- Message: review-aligned/override tool, then `remote` (myflow), `openai`, `openrouter`
- If all message attempts fail and fail-open is enabled, Flow uses a deterministic local fallback message.

Configuration:

```toml
[commit]
review_fail_open = true
message_fail_open = true
review_fallbacks = ["openrouter", "claude", "codex-high"]
message_fallbacks = ["remote", "openai", "openrouter"]
```

Environment overrides:
- `FLOW_COMMIT_REVIEW_FAIL_OPEN=0|1`
- `FLOW_COMMIT_MESSAGE_FAIL_OPEN=0|1`
- `FLOW_COMMIT_REVIEW_FALLBACKS="openrouter,claude,codex-high"`
- `FLOW_COMMIT_MESSAGE_FALLBACKS="remote,openai,openrouter"`

Codex -> GLM5 fallback example:

```toml
[commit]
review_fallbacks = ["glm5", "openrouter", "claude"]
message_fallbacks = ["glm5", "remote", "openai"]
```

`glm5` maps to the Rise/internal route with model `zai:glm-5`.

### Queue Policy

Queue only when review finds issues (auto-push on a clean review):

```toml
[commit]
queue = true
queue_on_issues = true
```

`--queue` / `--no-queue` still override this behavior.

### Commit Quality Gates (Testing + Required Skills)

Use local gates to block commits that skip tests or required workflow skills:

```toml
[commit.testing]
mode = "block"
runner = "bun"
bun_repo_strict = true
require_related_tests = true
ai_scratch_test_dir = ".ai/test"
run_ai_scratch_tests = true
allow_ai_scratch_to_satisfy_gate = false
max_local_gate_seconds = 20

[commit.skill_gate]
mode = "block"
required = ["quality-bun-feature-delivery"]

[commit.skill_gate.min_version]
quality-bun-feature-delivery = 2
```

When `mode = "block"`, `f commit` fails until the test/skill requirements are satisfied.  
For Bun repos, run checks with `bun bd test ...` for debug-build validation. If no related tracked tests are found, Flow can run tests under `ai_scratch_test_dir` (default `.ai/test`) as a fallback signal.

### AI Session Context

When `--context` is enabled, includes recent Claude Code session context:

```bash
# Default: 1000 tokens of context
f commit --context

# More context
f commit --context --tokens 3000
```

---

## Examples

### Quick Bug Fix

```bash
# Fix bug, commit with context
vim src/lib.rs
f commit --context -m "Fixes null pointer in edge case"
```

### Feature Branch

```bash
# Work on feature
git checkout -b feature/new-api
# ... make changes ...

# Commit without push
f commit -n

# More changes...
f commit -n

# Final push
git push -u origin feature/new-api
```

### Review Before Commit

```bash
# See what would be reviewed
f commit --dry

# If satisfied, commit
f commit
```

---

## Troubleshooting

### "Sensitive files staged"

Either:
1. Add files to `.gitignore`
2. Unstage with `git reset HEAD <file>`
3. Proceed anyway if intentional

### Review taking too long

Use `--sync` to see progress directly:
```bash
f commit --sync
```

### Hub not responding

Fall back to sync mode:
```bash
f commit --sync
```

## See Also

- [upstream](upstream.md) - Sync forks before committing
- [publish](publish.md) - Publish after committing
