# JJ Workspaces: Work on Multiple Branches Simultaneously

When you need to reference or work with code from another branch without disrupting your current work, use **jj workspaces**. This creates a second working copy of the same repo at a different path, pointed at a different revision.

## The Problem

You're on `feature-a` actively coding. You need to send another Claude session a prompt like "study the tracing code on `pr/main-fdb3446`" — but you can't check out that branch without losing your current working state.

## Solution: `jj workspace add`

```bash
cd ~/code/org/project
jj workspace add ../project-traces -r pr/main-fdb3446
```

Now you have two working copies sharing the same repo:

| Path | Branch | Use |
|------|--------|-----|
| `~/code/org/project` | `feature-a` | Your active work (untouched) |
| `~/code/org/project-traces` | `pr/main-fdb3446` | Full checkout for reference |

Point any tool or Claude session at the second path — it has all files on disk, no risk to your branch.

## Common Workflows

### Reference code from another branch

```bash
# Create workspace
jj workspace add ../project-ref -r some-branch

# Now another Claude session can freely explore:
# "study ~/code/org/project-ref — it has the tracing code"

# Clean up when done
jj workspace forget project-ref && rm -rf ../project-ref
```

### Cherry-pick files across branches

```bash
# No workspace needed — jj reads any revision directly:
jj file show src/lib/tracing.ts -r pr/main-fdb3446
jj diff --from main --to pr/main-fdb3446 --stat
```

### Work on two PRs at once

```bash
jj workspace add ../project-pr2 -r pr/feature-b
# Edit files in both directories independently
# Both share the same jj repo — commits are visible everywhere
```

## How It Works

- Both workspaces share the same `.jj/` repo backend (no git clone, no duplication)
- Each workspace has its own working copy commit (`@`)
- Changes committed in one workspace are immediately visible in the other via `jj log`
- The original workspace is completely unaffected

## Cleanup

```bash
# List workspaces
jj workspace list

# Remove a workspace (keeps the commits, removes the directory association)
jj workspace forget <name>
rm -rf ../project-ref
```

## When to Use What

| Need | Tool |
|------|------|
| Full directory for another tool/session to explore | `jj workspace add` |
| Read a specific file from another branch | `jj file show <path> -r <rev>` |
| See what changed on another branch | `jj diff --from main --to <branch>` |
| Compare two branches | `jj log -r 'branchA..branchB'` |
