# Moving Repos with Flow

How Flow manages repository locations, migration, and AI session continuity.

## Directory Layout

Flow uses two managed roots:

| Root | Purpose |
|------|---------|
| `~/code` | Active projects (`f code`) |
| `~/repos` | Cloned third-party repos (`f repos`) |
| `~/code/run` | Task-execution repos (`f r`, `f ri`, `f rp`) |

## Cloning Into the Right Place

### `f repos clone`

Clones GitHub repos into `~/repos/<owner>/<repo>`:

```bash
f repos clone owner/repo          # -> ~/repos/owner/repo
f repos clone https://github.com/owner/repo
```

Shallow clone by default with background full-history fetch. Auto-sets upstream remote for forks. Initializes jj with `--colocate`.

`~/repos` is immutable by default. Override with `FLOW_REPOS_ALLOW_ROOT_OVERRIDE=1`.

See [commands/repos.md](commands/repos.md) for full options.

### `f code`

Fuzzy-search git repos under `~/code` and open in editor:

```bash
f code         # fzf picker over ~/code
f code list    # list all repos under ~/code
```

## Moving a Project

### `f migrate` (primary command)

Moves or copies a project folder and automatically:
1. Moves/copies the directory (handles cross-device transparently)
2. Relinks `~/bin` symlinks pointing into the old path (move only)
3. Migrates Claude and Codex AI sessions to the new path

Three usage forms:

```bash
# Move current dir into ~/code/<relative>
cd ~/old/location/myproject
f migrate code myproject              # -> ~/code/myproject
f migrate code lang/rust/mylib        # -> ~/code/lang/rust/mylib

# Move current dir to any path
f migrate ~/code/stream

# Move a specific source to a target (no cd needed)
f migrate ~/code/lang/cpp/stream ~/code/stream
```

Options:

| Flag | Effect |
|------|--------|
| `--copy` / `-c` | Copy instead of move (keeps original) |
| `--dry-run` | Preview without writing |
| `--skip-claude` | Skip Claude session migration |
| `--skip-codex` | Skip Codex session migration |

Preview first:

```bash
f migrate --dry-run code stream
```

### What Happens to AI Sessions

Claude and Codex store project sessions keyed by filesystem path:

- **Claude**: `~/.claude/projects/<path-key>` directories are renamed
- **Codex**: `~/.codex/projects/<path-key>` directories are renamed, plus `.jsonl` session files under `~/.codex/sessions/` are updated in-place (the `cwd` field in `session_meta` records)

After migration a summary is printed:

```
Session migration summary:
  Claude project dirs moved: 1
  Codex legacy dirs moved: 1
  Codex jsonl files updated: 2
```

When copying (`--copy`), sessions are duplicated with a derived ID so both locations have independent history.

### `f code migrate` (alternative form)

Same as `f migrate code` but accessed through the `code` subcommand:

```bash
f code migrate ~/old/path myproject   # -> ~/code/myproject
```

### `f code move-sessions` (standalone session migration)

Migrate only AI sessions without moving any files:

```bash
f code move-sessions --from /old/path --to /new/path
f code move-sessions --from /old/path --to /new/path --dry-run
```

Useful when you moved a repo manually and need to fix sessions after the fact.

## Run Repos (`~/code/run`)

Run repos are a separate system for executing Flow tasks across multiple codebases without `cd`.

```bash
f r <task>                  # run in ~/code/run
f ri <task>                 # run in ~/code/run/i
f rp <project> <task>       # run in ~/code/run/<project> (falls back to i/<project>)
f rip <project> <task>      # run in ~/code/run/i/<project>
```

Management:

```bash
f run-load <name> <url>     # clone/update a run repo
f run-sync                  # sync all run repos
f run-list                  # list all run repos
```

See [run-repos.md](run-repos.md) for full details.

## Common Workflows

### Move a project into `~/code`

```bash
cd ~/downloads/cool-project
f migrate code cool-project
# -> ~/code/cool-project with sessions migrated
```

### Reorganize nested projects

```bash
f migrate ~/code/lang/cpp/stream ~/code/stream
# Directory moved, ~/bin symlinks updated, sessions migrated
```

### Clone a fork with upstream tracking

```bash
f repos clone myfork/repo
# -> ~/repos/myfork/repo
# upstream auto-detected via gh API, jj initialized
```

### Copy a project for experimentation

```bash
f migrate --copy ~/code/app ~/code/app-experiment
# Original untouched, sessions duplicated with new IDs
```

### Fix sessions after a manual move

```bash
mv ~/code/old ~/code/new
f code move-sessions --from ~/code/old --to ~/code/new
```

## See Also

- [commands/repos.md](commands/repos.md) — `f repos clone` / `f repos create`
- [commands/migrate.md](commands/migrate.md) — `f migrate` full reference
- [run-repos.md](run-repos.md) — run repo shortcuts
