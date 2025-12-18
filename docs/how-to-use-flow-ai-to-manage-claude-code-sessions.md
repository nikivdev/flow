# Managing Claude Code Sessions with Flow

Flow provides built-in session management for Claude Code, allowing you to track, save, and resume AI coding sessions per project.

## Quick Start

```bash
# Fuzzy search sessions and resume one
f ai

# Import all existing sessions from ~/.claude
f ai import

# Save the most recent session with a name
f ai save my-feature

# Resume a specific session
f ai resume my-feature
```

## Commands

### `f ai`

Opens a fuzzy finder to search and select a session to resume.

```
ai>
3d ago | in here when i run `f` it does now show tasks from the flow…
6d ago | when doing `f` and doing fuzzy search i can now do ctrl+f t…
1w ago | just tried to run a task and then ran `f last-cmd` but it g…
1w ago | want to add support for nested flow.toml
1w ago | get issue in project where it does not accept input from fl…
2w ago | normal tasks work
2w ago | in spec/tracing-flow.md create plan how to do this
```

- Type to filter sessions by content
- Press Enter to resume the selected session
- Press Esc to cancel
- Sessions show relative time (`3d ago`, `1w ago`, `yesterday`) and first message

If you've saved a session with a custom name, it appears with the name:
```
my-feature | 3d ago | in here when i run `f` it does now show tasks…
```

### `f ai import`

Bulk imports all existing Claude sessions for the current project from `~/.claude/projects/`.

```bash
f ai import
```

This will:
- Initialize the `.ai/` folder structure if needed
- Scan Claude's storage for sessions in this project
- Auto-generate internal names for tracking
- Skip sessions that are already imported

### `f ai save <name>`

Saves the most recent session with a custom name for easy recall.

```bash
f ai save auth-feature
```

Options:
- `--id <session-id>` - Save a specific session instead of the most recent

```bash
f ai save old-feature --id f0d978c9
```

### `f ai resume [session]`

Resumes a Claude Code session directly (without fuzzy finder). Accepts:
- Session name: `f ai resume auth-feature`
- Session ID prefix (8+ chars): `f ai resume 3f34ac70`
- No argument (resumes most recent): `f ai resume`

```bash
# Resume by saved name
f ai resume auth-feature

# Resume most recent session
f ai resume
```

### `f ai notes <session>`

Opens or creates a markdown notes file for a session in your `$EDITOR`.

```bash
f ai notes auth-feature
```

Creates `.ai/sessions/claude/notes/auth-feature.md`:
```markdown
# Session: auth-feature

Session ID: 3f34ac70

## Notes

(your notes here)
```

Use this to document:
- What the session accomplished
- Key decisions made
- Follow-up tasks
- Context for future reference

### `f ai remove <session>`

Removes a saved session from tracking (doesn't delete the actual Claude session).

```bash
f ai remove old-feature
```

### `f ai init`

Initializes the `.ai/` folder structure in the current project.

```bash
f ai init
```

Creates:
```
.ai/
.ai/sessions/claude/index.json
.ai/sessions/claude/notes/
.ai/.gitignore
```

This is automatically called by `f ai import` and `f ai save`.

## File Structure

```
your-project/
├── .ai/
│   ├── .gitignore              # Ignores notes/ by default
│   └── sessions/
│       └── claude/
│           ├── index.json      # Saved session metadata
│           └── notes/          # Personal session notes
│               └── auth-feature.md
```

## Git Integration

By default:
- `.ai/sessions/claude/index.json` is tracked (share session names with team)
- `.ai/sessions/claude/notes/` is gitignored (personal notes)

To track notes with git, edit `.ai/.gitignore`.

## How It Works

Flow reads Claude Code's session storage at `~/.claude/projects/<project-path>/` where each session is stored as a `.jsonl` file with conversation history.

When you run `f ai`, Flow:
1. Converts your current directory to Claude's project folder name
2. Reads all session files in that folder
3. Extracts timestamps and first messages
4. Shows relative times (3d ago, 1w ago, yesterday)
5. Opens fzf for fuzzy selection
6. Resumes the selected session with `claude --resume`

## Tips

1. **Quick resume**: Just run `f ai`, type a few chars to filter, press Enter
2. **Name important sessions**: `f ai save feature-name` after productive work
3. **Pick up where you left off**: `f ai resume` continues the most recent session
4. **Document decisions**: Use `f ai notes` to capture context for future you
