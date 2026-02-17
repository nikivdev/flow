# f recipe

Search and execute markdown recipes.

Recipes are markdown files with frontmatter + an executable shell code block.

## Usage

```bash
f recipe list
f recipe search <query>
f recipe run <selector>
f recipe init
```

## Options

- `--scope <project|global|all>`: recipe source scope (default `all`).
- `--global-dir <PATH>`: override global recipes directory.
- `--cwd <PATH>` (run only): working directory for execution.
- `--dry-run` (run only): print command without executing.

## Recipe locations

- Project recipes: `.ai/recipes/project` (fallback `.ai/recipes`).
- Global recipes: `~/.config/flow/recipes`.

## Flow release/install examples

```bash
cd ~/code/flow
f recipe list --scope project
f recipe run project:release-flow-registry --dry-run
f recipe run project:install-rise-auto
```

