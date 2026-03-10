# Fast Private Repo Creation From An Existing Local Checkout

Use this when you already have code locally and want a private GitHub repo quickly.

This guide covers two different cases:

1. the current folder should become its own private GitHub repo
2. the current folder already has `origin`/`upstream`, and you want an extra private share remote like `<repo>-i`

The important distinction:

- `f publish` works from the current directory and is not tied to `~/repos`
- `f repos clone` is the command that cares about `~/repos`

So yes, this works from places like:

- `~/repos/viperrcrypto/Siftly`
- `~/code/flow-extension`

## Fastest path: current folder becomes a private repo

Use this when:

- the folder is your project
- you want GitHub to become `origin`
- you do not need to preserve some existing public `origin`/`upstream` setup

From the repo root:

```bash
f publish -y --private
```

This is the fastest default.

What it does:

- checks `gh` auth
- initializes git if needed
- creates an initial commit if the repo has none
- creates the private GitHub repo
- wires/pushes the current project

Example from outside `~/repos`:

```bash
cd ~/code/flow-extension
f publish -y --private
```

That is the recommended path for repos like `~/code/flow-extension`.

## Fast private share repo while keeping existing origin/upstream

Use this when:

- the repo already has a real public `origin` or `upstream`
- you want a separate private mirror/share repo
- you do not want to disturb existing remotes

This is the right pattern for repos like:

```text
~/repos/viperrcrypto/Siftly
```

### Recommended command sequence

1. Make sure your intended work is committed.

```bash
git status --short --branch
git add <intended files>
git commit -m "<message>"
```

2. Sync with `origin/main` if needed.

```bash
git fetch origin main
git rev-parse HEAD origin/main
```

If you need to move your work on top of `origin/main`, do that intentionally before publishing.

3. Create the private repo and add it as a separate remote.

```bash
gh repo create nikivdev/<repo>-i --private --disable-wiki --source=. --remote=private
```

4. Push your current commit or branch to the new private repo.

```bash
git push -u private HEAD:main
```

Example:

```bash
cd ~/repos/viperrcrypto/Siftly
gh repo create nikivdev/Siftly-i --private --disable-wiki --source=. --remote=private
git push -u private HEAD:main
```

This leaves:

- `origin` alone
- `upstream` alone
- `private` as the new share/mirror remote

## When to use `f publish` vs `gh repo create`

Use `f publish` when:

- you want the current folder to become the repo
- you want the simplest path
- the repo is new or self-owned

Use `gh repo create ... --remote=private` when:

- the checkout already tracks a public repo
- you want a separate private mirror
- you want to share WIP without changing `origin`

## Safe default for a repo with an existing public origin

If a repo already has `origin` and you are not completely sure what to do, use this:

```bash
git remote -v
git fetch origin main
gh repo create nikivdev/<repo>-i --private --disable-wiki --source=. --remote=private
git push -u private HEAD:main
```

That is the safest default for “share this work privately with another dev”.

## Optional: make Flow push to the private remote by default

If you want future `f sync --push` calls to go to the private repo instead of `origin`, add this to the repo’s `flow.toml`:

```toml
[git]
remote = "private"
```

Only do this if the repo is now private-first for your workflow.

If you just want a one-off share snapshot, skip this.

## Common examples

### Example: private repo from `~/code/flow-extension`

```bash
cd ~/code/flow-extension
f publish -y --private
```

### Example: private share mirror from `~/repos/viperrcrypto/Siftly`

```bash
cd ~/repos/viperrcrypto/Siftly
git fetch origin main
gh repo create nikivdev/Siftly-i --private --disable-wiki --source=. --remote=private
git push -u private HEAD:main
```

## Troubleshooting

### `gh` is not authenticated

Run:

```bash
gh auth login
gh auth status
```

### Repo already exists

If the private repo already exists, skip creation and just wire or verify the remote:

```bash
git remote add private git@github.com:nikivdev/<repo>-i.git
git push -u private HEAD:main
```

If `private` already exists:

```bash
git remote -v
git push -u private HEAD:main
```

### I am outside `~/repos`

That is fine.

`f publish` operates on the current folder, not on `~/repos`.

### I do not want to push uncommitted changes

Good. Commit first.

For existing repos with real history, do not rely on auto-magic here. Make the commit you want to share, then push that exact commit.

## Related docs

- [commands/publish.md](commands/publish.md)
- [commands/repos.md](commands/repos.md)
- [private-fork-flow.md](private-fork-flow.md)
- [private-mirror-sync-workflow.md](private-mirror-sync-workflow.md)
