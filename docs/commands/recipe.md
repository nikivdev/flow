# f recipe

Legacy compatibility command.

Preferred model:

- Put shell tasks in `flow.toml` under `[[tasks]]`.
- Put AI/native tasks in `.ai/tasks/*.mbt`.
- Run via `f <task>` or `f ai:<selector>`.

`f recipe` remains for older repos that still use `.ai/recipes`.

## Usage

```bash
f recipe list          # legacy listing
f recipe run <selector> # legacy execution
```

## Options

- `--scope <project|global|all>`: recipe source scope (default `all`)
- `--global-dir <PATH>`: override global recipes directory
- `--cwd <PATH>` (run only): working directory for execution
- `--dry-run` (run only): print command without executing

## Legacy Recipe Locations

- Project recipes: `.ai/recipes/project` (fallback `.ai/recipes`).
- Global recipes: `~/.config/flow/recipes`.

Supported extensions: `.md`, `.markdown`, `.mbt`

MoonBit recipe metadata is optional and can be declared in top comment lines:

```mbt
// title: My Fast Recipe
// description: Run a moonbit action quickly
// tags: [moonbit, fast]
```

## Migration

```bash
# Old
f recipe run project:my-recipe

# New
f tasks init-ai
f ai:my-task
```
