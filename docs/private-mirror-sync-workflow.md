# Private Mirror Sync Workflow (Upstream/Public + Private Fork)

Use this when you work in a public fork clone locally but want to keep your full WIP history in a private mirror repo.

Example mapping used here:

- Local repo: `~/repos/pqrs-org/Karabiner-Elements-user-command-receiver`
- Public remotes:
  - `origin` = `nikivdev/Karabiner-Elements-user-command-receiver`
  - `upstream` = `pqrs-org/Karabiner-Elements-user-command-receiver`
- Private mirror remote:
  - `private` = `nikivdev/Karabiner-Elements-user-command-receiver-i`

## Goal

1. Move feature-branch work onto `main` locally.
2. Sync with latest `origin/main`.
3. Push local `main` to a private fork/mirror.

## Recommended commands

### 1) Save dirty working tree and move branch commits to `main`

```bash
# from repo root
git stash push -u -m "move-to-main"
git switch main
git merge --ff-only <feature-branch>
```

If `--ff-only` fails, do a normal merge or cherry-pick intentionally.

### 2) Sync with `origin/main`

If you want strict origin-only sync (no upstream automation):

```bash
git fetch origin --prune
git rebase origin/main
```

Then reapply stash:

```bash
git stash apply stash@{0}
```

### 3) Commit only intended files

Avoid runtime artifacts (`out/logs/*`, todo scratch files, etc.).

```bash
git add -A -- ':!out/logs/cli.log' ':!out/logs/trace.log' ':!.ai/todos/todos.json'
git commit -m "<message>"
```

### 4) Create private mirror repo and push

```bash
# one-time creation
gh repo create nikivdev/<repo>-i --private --source=. --remote=private --disable-wiki

# publish branch
git push -u private main
```

## Flow-specific notes (`f sync`)

`f sync` can use jj integration and may rebase against upstream depending on repo setup.
That is useful in normal fork workflows, but if you need strict origin-only syncing for a private mirror flow, prefer explicit Git commands:

```bash
git fetch origin --prune
git rebase origin/main
```

Then use Flow for commit/review as usual.

## If `f sync` creates jj conflict artifacts

Symptoms:

- `jj` conflict commits
- files like `.jjconflict-base-*` / `.jjconflict-side-*`

Recovery pattern:

1. stash uncommitted work (`git stash push -u`)
2. create clean branch from `origin/main`
3. cherry-pick your intended commits
4. reapply stash
5. continue from clean branch and move pointer back to `main`

```bash
git stash push -u -m "recovery"
git fetch origin --prune
git switch -c main-clean origin/main
git cherry-pick <commit1> <commit2>
git stash apply <stash-with-real-work>
git branch -f main main-clean
git switch main
```

## Optional: keep `main` tracking private mirror

If this repo is now private-first for your local work:

```bash
git push -u private main
```

and keep `origin`/`upstream` as fetch sources for rebases.
