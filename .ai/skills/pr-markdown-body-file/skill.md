---
name: pr-markdown-body-file
description: Always edit/create GitHub PR bodies from a markdown file; never inline escaped newlines.
version: 1
source: flow-default
---

# PR Markdown Body Discipline

Use this skill when creating or editing pull requests.

## Rules (Must)

- Never pass multi-line PR text via a quoted CLI string with `\n`.
- Always use a markdown file with `--body-file`.
- Prefer `f pr open edit` when available for local file-driven PR editing.

## Why

Inline escaped newlines are fragile and can produce literal `\n` text in GitHub PR descriptions.
`--body-file` is deterministic and reviewable.

## Safe patterns

### Create PR (GitHub CLI)

```bash
cat > /tmp/pr-body.md <<'EOF'
## Summary
- ...

## Scope
- ...
EOF

gh pr create --title "..." --body-file /tmp/pr-body.md
```

### Edit existing PR

```bash
cat > /tmp/pr-body.md <<'EOF'
## Summary
- ...
EOF

gh pr edit <number> --body-file /tmp/pr-body.md
```

### Flow-native edit loop

```bash
f pr open edit
```

This opens a local markdown file and syncs title/body on save.

## Verification

- Run:

```bash
gh pr view <number> --json body -q .body
```

- Confirm markdown renders correctly and contains no literal `\n`.
