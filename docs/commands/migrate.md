# f migrate

Move or copy a project folder to a new location, preserving symlinks and AI sessions.

## Overview

Relocates a project directory and automatically migrates Claude and Codex AI sessions so conversation history follows the project. Also relinks any `~/bin` symlinks that pointed into the old path (move only).

## Quick Start

```bash
# Move current directory into ~/code/lang/cpp/stream
cd ~/code/lang/cpp/stream
f migrate code stream

# Move current directory to an arbitrary path
f migrate ~/code/stream

# Move a specific source to a target
f migrate ~/code/lang/cpp/stream ~/code/stream

# Preview what would happen (no changes)
f migrate --dry-run code stream

# Copy instead of move (keeps original)
f migrate --copy code stream
```

## Your case

To migrate `~/code/lang/cpp/stream` to `~/code/stream`:

```bash
cd ~/code/lang/cpp/stream
f migrate code stream
```

Or without `cd`:

```bash
f migrate ~/code/lang/cpp/stream ~/code/stream
```

Preview first with `--dry-run`:

```bash
f migrate --dry-run ~/code/lang/cpp/stream ~/code/stream
```

## Usage Forms

### `f migrate code <relative>`

Moves the current directory into `~/code/<relative>`. This is the most common form.

```bash
cd ~/old/location/myproject
f migrate code myproject          # -> ~/code/myproject
f migrate code lang/rust/mylib    # -> ~/code/lang/rust/mylib
```

### `f migrate <target>`

Moves the current directory to `<target>` (any absolute or relative path).

```bash
cd ~/old/location/myproject
f migrate ~/code/myproject
```

### `f migrate <source> <target>`

Moves `<source>` to `<target>` without needing to `cd` first.

```bash
f migrate ~/code/lang/cpp/stream ~/code/stream
```

## Options

| Option | Short | Description |
|--------|-------|-------------|
| `--copy` | `-c` | Copy instead of move (keeps the original intact) |
| `--dry-run` | | Show what would change without writing anything |
| `--skip-claude` | | Skip migrating Claude Code sessions |
| `--skip-codex` | | Skip migrating Codex sessions |

## What Happens

### Step 1: Move or copy the folder

The project directory is moved (or copied with `--copy`) to the target path. Parent directories are created automatically. Cross-device moves are handled transparently (copy + delete).

### Step 2: Relink ~/bin symlinks (move only)

Any symlinks in `~/bin` that pointed into the old path are updated to point to the new location. Skipped when using `--copy`.

### Step 3: Migrate AI sessions

Claude and Codex store project sessions keyed by filesystem path. Migrate updates these so conversation history is preserved at the new location.

- **Claude sessions**: Project directories under `~/.claude/projects/` are renamed from the old path-based key to the new one.
- **Codex sessions**: Legacy project directories are renamed, and `.jsonl` session files referencing the old path are updated in-place.

A summary is printed showing how many session dirs/files were migrated.

## Examples

```bash
# Move project into ~/code, nested path
cd ~/downloads/cool-project
f migrate code tools/cool-project

# Copy a project (keeps original)
f migrate --copy ~/code/app ~/backup/app

# Dry run to preview
f migrate --dry-run code stream

# Move without migrating AI sessions
f migrate --skip-claude --skip-codex ~/old/path ~/new/path

# Move and only migrate Claude (skip Codex)
f migrate --skip-codex code myproject
```

## Troubleshooting

### "Destination already exists"

The target path must not exist. Remove or rename the existing directory first, or choose a different target.

### "Source and destination are the same path"

Both paths resolve to the same location. Double-check your arguments.

### Session migration warnings

After a move, you may see warnings like:

```
WARN Claude session dir still present: ...
WARN Codex sessions still reference the old path:
  /path/to/file.jsonl
```

These mean some sessions couldn't be fully migrated. You can manually inspect or delete the referenced files.

## See Also

- [repos](repos.md) - Clone repositories into structured layout
