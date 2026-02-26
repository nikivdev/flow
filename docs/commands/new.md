# f new

Create a new project from a local starter template under `~/new`.

## Overview

`f new` copies a directory from `~/new/<template>` into a destination path.

This is the native Flow way to work with local starters you keep in `~/new`.

## Usage

```bash
f new [template] [path]
```

- `template`: folder name inside `~/new` (for example `app`, `docs`, `web`)
- `path`: destination path (optional)

If `template` is omitted, Flow opens an `fzf` picker from templates in `~/new`.

## Path Resolution Rules

Flow resolves the destination path like this:

```bash
f new app          # -> ./app
f new app zerg     # -> ~/code/zerg
f new app ./xn     # -> ./xn
f new app ~/xn     # -> ~/xn
```

Notes:
- Plain names (no `./`, `../`, `/`, `~`) are treated as `~/code/<name>`.
- Use `~/...` or absolute paths for custom locations outside `~/code`.

## Dry Run

Preview copy behavior without writing files:

```bash
f new app ~/xn --dry-run
```

## Starter Workflow

1. Create or update starter in `~/new/<template>`.
2. Generate a new project with `f new <template> <target>`.
3. Enter the new project and run its setup/dev tasks with Flow.

Example:

```bash
f new app ~/xn
cd ~/xn
f tasks
```

## Common Errors

- `Template not found`: `~/new/<template>` does not exist.
- `Destination already exists`: remove/rename target path or choose a new destination.
