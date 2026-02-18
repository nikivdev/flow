# f invariants

Validate project invariants declared in `flow.toml`.

This command checks the `[invariants]` section and reports violations in changed code.

## Usage

```bash
# Check all local changes vs HEAD
f invariants

# Check only staged changes
f invariants --staged
```

## What It Checks

1. `forbidden`: disallowed patterns in added diff lines.
2. `deps.approved`: unapproved dependencies in `package.json` sections (`dependencies`, `devDependencies`, `peerDependencies`).
3. `files.max_lines`: changed files exceeding the configured line limit.

## Modes

Set in `flow.toml`:

- `mode = "off"`: disable checks.
- `mode = "warn"`: print findings, exit success.
- `mode = "block"`: fail command when blocking findings exist.

## Example `flow.toml`

```toml
[invariants]
mode = "block"
architecture_style = "layered monorepo, event-driven core"
non_negotiable = [
  "no inline imports",
  "no any types unless justified",
]
forbidden = [
  "git add -A",
  "git reset --hard",
]

[invariants.terminology]
"pi-ai" = "LLM abstraction layer"
"pi-agent" = "stateful agent runtime"

[invariants.deps]
policy = "approval_required"
approved = ["@sinclair/typebox", "@reatom/core"]

[invariants.files]
max_lines = 300
```

## Commit Integration

`f commit` runs the invariant gate during commit-with-check flow.

- In `mode = "block"`, commits are blocked on invariant warnings/critical findings.
- Invariant context and findings are injected into AI review prompts.

