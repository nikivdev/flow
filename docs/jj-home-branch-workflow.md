# JJ Home-Branch Workflow

This workflow is for teams or individuals who keep one long-lived personal branch on top of trunk,
then stack short-lived task branches on top of that branch.

Flow supports that model directly through:

- `f status` for a workflow-aware status view
- `jj.home_branch` in `flow.toml`
- `f sync` home-branch mode for pulling `origin/<default-branch>` into the home branch
- `f jj workspace review <branch>` for isolated branch-specific working copies

## Mental model

Use three layers:

1. trunk: `main`
2. home branch: your long-lived integration branch
3. leaf branches: `review/*`, `codex/*`, or other task branches that sit on top of the home branch

The default checkout usually stays on the home branch. Branch-specific work happens in isolated
JJ workspaces.

## Config

```toml
[jj]
default_branch = "main"
home_branch = "alice"
```

For `f sync`, if `jj.home_branch` is omitted, Flow falls back to the basename of `$HOME` and then
`USER` / `USERNAME`.

## Status as the preflight

Before switching branches, creating workspaces, committing, or publishing, run:

```bash
f status
```

This should tell you:

- current workspace and path
- current branch and its role
- configured home branch
- whether you are on the home branch or a leaf branch
- which `review/*` and `codex/*` branches currently sit on top of the home branch
- whether the working copy is clean enough to mutate safely

Use raw `f jj status` only when you need the underlying Jujutsu status output.

## Default operating pattern

Keep the main checkout on the home branch:

```bash
cd ~/code/org/project
f status
```

Create or reuse an isolated workspace for branch-specific work:

```bash
f jj workspace review review/alice-feature
cd ~/.jj/workspaces/project/review-alice-feature
f status
```

Inside the workspace, use `jj` or `f jj`.

## Why this is safer

- The main checkout stays stable.
- Branch-specific edits do not mix with unrelated home-branch changes.
- A second Codex or Claude session can work in the review workspace without disturbing the main
  checkout.
- `f status` provides one consistent summary instead of forcing the user or agent to infer state
  from several lower-level commands.

## Publish boundary

The important distinction is not just the branch name. It is also where the work lives:

- Git branch checkout
- JJ review workspace

Be explicit about the publish path. Do not assume a colocated Git checkout and a JJ workspace are
interchangeable.

## Recommended rule set

- default checkout: home branch
- task work: review workspace
- preflight before mutation: `f status`
- inspect raw JJ details only when needed: `f jj status`

That keeps the workflow legible to both humans and coding agents.
