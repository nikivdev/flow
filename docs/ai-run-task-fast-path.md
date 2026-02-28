# AI Run Task Fast Path

Goal: let AI add a run task in one edit, return a runnable `f ...` command, and optionally push.

## Prompt Contract

```text
make public task <doc-path.md> <thing>
make internal task <doc-path.md> <thing>
```

`<doc-path.md>` is implementation context. Do not edit docs unless asked.

## File Targeting

- Public: `~/run/flow.toml`
- Internal shared: `~/run/i/flow.toml`
- Internal project (only if project is explicitly requested): `~/run/i/<project>/flow.toml`

## Fast Rules

- Read only:
  - `<doc-path.md>`
  - target `flow.toml`
- Edit one `flow.toml` file.
- Add one `[[tasks]]` block unless user asks for multiple.
- Required fields: `name`, `description`, `command`.
- Add `interactive = true` only for prompts/TUI.
- Use `"$@"` passthrough when args should flow through.
- Keep shell idempotent and non-destructive.

## Required AI Reply Format

```text
Changed:
- /abs/path/to/flow.toml

Run:
f <r|ri|rip ...> <task-name> [args]

Share:
cd <repo> && git add <flow.toml> && git commit -m "add <task-name>" && git push
```

Shortcut map:
- Public -> `f r <task>`
- Internal shared -> `f ri <task>`
- Internal project -> `f rip <project> <task>`

## Push Confidence Gate

Include `Share` command only when all are true:

- single-file `flow.toml` change
- no secrets
- no destructive commands
- no uncertain behavior

Otherwise:

```text
Share:
skip (manual review)
```
