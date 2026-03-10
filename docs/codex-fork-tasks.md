# Codex Fork Tasks

These Flow tasks automate the personal Codex fork workflow described in:

- `~/docs/codex/codex-fork-home-branch-workflow.md`

They are intentionally narrow:

- keep `~/repos/openai/codex` as the upstream reference checkout
- keep `~/repos/nikivdev/codex` as the fork home checkout
- create one stable worktree per real fork task under `~/.worktrees/codex`
- attach Codex sessions to that worktree instead of to one drifting checkout

## Commands

Run these from `~/code/flow`:

```bash
cd ~/code/flow
f codex-fork-status
f codex-fork-sync
f codex-fork-task "add workspace ref to the footer"
f codex-fork-last
f codex-fork-promote --push
```

If you are outside `~/code/flow`, use the explicit config form:

```bash
flow run --config ~/code/flow/flow.toml codex-fork-task "add workspace ref to the footer"
```

## What Each Task Does

### `f codex-fork-status`

Shows:

- the upstream reference checkout
- the personal fork home checkout
- the current `nikiv`, `upstream/main`, and `private/nikiv` refs
- existing fork worktrees
- the last worktree used by the helper

Use this first when the fork state is unclear.

### `f codex-fork-sync`

Fast-forwards `nikiv` in `~/repos/nikivdev/codex` to `upstream/main`.

Optional push:

```bash
f codex-fork-sync --push
```

Safety rule:

- it refuses to run if the fork home checkout is dirty

### `f codex-fork-task "<query>"`

This is the main entry point.

It:

1. derives a branch like `codex/<slug>`
2. creates or reuses `~/.worktrees/codex/<branch-name-with-slashes-rewritten>`
3. records that worktree as the "last fork worktree"
4. resumes the last Codex session in that worktree if one exists
5. otherwise starts a fresh Codex session there with an initial prompt that points at the fork workflow doc

Examples:

```bash
f codex-fork-task "add workspace ref to the footer"
f codex-fork-task "thread name startup" --branch codex/thread-name-startup
f codex-fork-task "statusline workspace ref" --no-launch
```

### `f codex-fork-last`

Resumes `codex resume --last` in the last worktree created or used by the helper.

This is the closest current Flow equivalent to binding a key that reattaches to the last active fork session.

You can also target a branch or path explicitly:

```bash
f codex-fork-last codex/workspace-awareness
f codex-fork-last ~/.worktrees/codex/codex-workspace-awareness
```

### `f codex-fork-promote`

Creates or updates a review branch from the current task worktree tip.

Default mapping:

- `codex/workspace-awareness` -> `review/nikiv-workspace-awareness`

Examples:

```bash
f codex-fork-promote
f codex-fork-promote codex/workspace-awareness --push
f codex-fork-promote ~/.worktrees/codex/codex-workspace-awareness --review-branch review/nikiv-codex-workspace-awareness
```

## State File

The helper records the last used worktree in:

```text
~/.flow/codex-fork/last-worktree.txt
```

That is what powers `f codex-fork-last`.

## Environment Overrides

If you want the helper to point somewhere else, override these env vars:

- `FLOW_CODEX_UPSTREAM_CHECKOUT`
- `FLOW_CODEX_FORK_HOME`
- `FLOW_CODEX_WORKTREE_ROOT`
- `FLOW_CODEX_WORKFLOW_DOC`
- `FLOW_CODEX_FORK_STATE_DIR`
- `FLOW_CODEX_FORK_BASE_BRANCH`
- `FLOW_CODEX_FORK_PRIVATE_REMOTE`
- `FLOW_CODEX_FORK_UPSTREAM_REMOTE`
- `FLOW_CODEX_FORK_UPSTREAM_BRANCH`
