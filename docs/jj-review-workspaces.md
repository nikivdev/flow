# JJ Review Workspaces

When you need an isolated working copy for a review branch, use a **JJ review workspace** instead
of switching your current checkout or creating an ad hoc temporary worktree.

This is the safest way to inspect, edit, and validate a review branch while leaving your current
repo state untouched.

## Command

```bash
cd ~/code/org/project
f status
f jj workspace review review/alice-feature
```

This creates or reuses a stable workspace at:

```bash
~/.jj/workspaces/project/review-alice-feature
```

Then work there:

```bash
cd ~/.jj/workspaces/project/review-alice-feature
f status
```

## Why Use This

- Your current checkout stays exactly as it is.
- The workspace path is stable and reusable.
- Another Codex or Claude session can work inside the review workspace safely.
- You avoid mixing “temporary scratch path” decisions into your review flow.
- `f status` makes the home-branch versus review-workspace role explicit before you mutate anything.

## How It Resolves The Base

`f jj workspace review <branch>` chooses the workspace base in this order:

1. `--base <rev>` if you passed one
2. the local Git branch commit for `<branch>`
3. the remote Git branch commit for `<remote>/<branch>`
4. trunk (`<default_branch>` or `<default_branch>@<remote>`)

That makes the command useful both before and after the review branch exists locally.

## Reuse Behavior

If the review workspace already exists, Flow reuses it instead of creating another copy.

That means this is safe to run repeatedly:

```bash
f jj workspace review review/alice-feature
```

You get one stable place for that branch, not a pile of temporary directories.

## Important Caveat In Colocated Repos

Use `jj` or `f jj` inside the review workspace.

In a colocated repo, plain `git` still points at the main Git checkout, not the JJ workspace's
working-copy commit. Because of that, `f jj workspace review` intentionally does **not** try to run
branch-switching logic for you.

Recommended rule:

- inside the review workspace: use `jj` / `f jj`
- in the main checkout: use your normal Git or Flow branch-switch flow

## Example Workflow

```bash
# Create or reuse the review workspace
f jj workspace review review/alice-feature

# Move into it
cd ~/.jj/workspaces/project/review-alice-feature

# Inspect state
jj st
jj log -r @

# Make edits and commit as usual
jj describe -m "Adjust runtime startup behavior"
jj new
```

If you want a tracked bookmark for publishing later:

```bash
f jj bookmark create review/alice-feature --rev @ --track --remote origin
```

## Cleanup

When you no longer need the workspace:

```bash
jj workspace list
jj workspace forget review-alice-feature
rm -rf ~/.jj/workspaces/project/review-alice-feature
```

## When To Use This vs `lane`

Use `f jj workspace lane <name>` when you want a new parallel line of work anchored from trunk.

Use `f jj workspace review <branch>` when the workspace should correspond to a specific review
branch and keep a stable branch-derived path.
