# Private Fork Flow Runbook

Use this as the default AI-safe procedure when work must be pushed to a private fork, not public `origin`.

## Goal

- Keep upstream/public remotes for syncing.
- Push writable changes to a private remote.
- Make `f sync --push` and commit flows consistently target the private remote.

## One-Time Setup Per Repo

1. Add private remote.

```bash
cd <repo-dir>
git remote add <private-remote> git@github.com:<your-user>/<repo>-i.git
git fetch <private-remote>
```

2. Set Flow writable remote in `flow.toml`.

```toml
[git]
remote = "<private-remote>"
```

3. Verify remote map.

```bash
git remote -v
```

Expected pattern:
- `origin` and/or `upstream` are read/sync sources.
- `<private-remote>` is writable push target.

## Standard Push Procedure

```bash
cd <repo-dir>
git status --short --branch
git diff --stat
git diff
f commit
f sync --push
```

Flow behavior:
- `f sync --push` uses `[git].remote` when configured.
- Fallback order is `[git].remote`, then legacy `[jj].remote`, then `origin`.

## AI Guardrails (Must Follow)

- Never push before reviewing `git status --short --branch` and `git diff --stat`.
- Never include unrelated generated artifacts in the commit.
- If the tree is noisy, create smaller focused commits before push.
- If the remote target is unclear, stop and verify `flow.toml` `[git].remote` plus `git remote -v`.

## Quick Validation

```bash
git config --get branch.$(git rev-parse --abbrev-ref HEAD).remote || true
git remote get-url <private-remote>
```

Then run:

```bash
f sync --push
```

## Related Docs

- `docs/commands/sync.md`
- `docs/flow-toml-spec.md`
- `docs/private-mirror-sync-workflow.md`
- `docs/commands/upstream.md`
