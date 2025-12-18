# Managing Claude Code Sessions with Flow

Flow provides built-in session management for Claude Code, allowing you to track, save, and resume AI coding sessions per project.

## Quick Start

```bash
# List all Claude sessions for current project
f ai

# Import all existing sessions from ~/.claude
f ai import

# Resume a session by name or number
f ai resume my-feature
f ai resume 1

# Save the most recent session with a name
f ai save my-feature
```

## Commands

### `f ai` / `f ai list`

Lists all Claude Code sessions for the current project.

```
Saved sessions:
────────────────────────────────────────────────────────────
  my-feature (3f34ac70) Adding user authentication flow

Recent Claude sessions:
────────────────────────────────────────────────────────────
  1. 3f34ac70 (2025-12-15 08:22) *
     Adding user authentication flow...
  2. f0d978c9 (2025-12-12 12:41)
     Fix database connection pooling...
```

- Saved sessions appear at the top with their custom names
- Recent sessions show with a number for quick reference
- Sessions marked with `*` are already saved

### `f ai import`

Bulk imports all existing Claude sessions for the current project from `~/.claude/projects/`.

```bash
f ai import
```

This will:
- Initialize the `.ai/` folder structure if needed
- Scan Claude's storage for sessions in this project
- Auto-generate names from date + first message words
- Skip sessions that are already imported

Example output:
```
Imported: 20251215-adding-auth-flow (3f34ac70)
Imported: 20251212-fix-database (f0d978c9)
Imported: 20251209-refactor-api (a284b9a5)

Imported 3 sessions, skipped 0 (already exists)
```

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

Resumes a Claude Code session. Accepts:
- Session name: `f ai resume auth-feature`
- Session ID prefix (8+ chars): `f ai resume 3f34ac70`
- List number: `f ai resume 1`
- No argument (resumes most recent): `f ai resume`

```bash
# Resume by saved name
f ai resume auth-feature

# Resume by number from list
f ai resume 2

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
│               ├── auth-feature.md
│               └── refactor.md
```

### index.json

Stores saved session metadata:
```json
{
  "sessions": {
    "auth-feature": {
      "id": "3f34ac70-7baa-4b75-913f-67f0e168b1f4",
      "description": "Adding user authentication flow with OAuth2...",
      "saved_at": "2025-12-15T16:22:00Z",
      "last_resumed": null
    }
  }
}
```

## Git Integration

By default:
- `.ai/sessions/claude/index.json` is tracked (share session names with team)
- `.ai/sessions/claude/notes/` is gitignored (personal notes)

To track notes with git, edit `.ai/.gitignore`.

## How It Works

Flow reads Claude Code's session storage at `~/.claude/projects/<project-path>/` where each session is stored as a `.jsonl` file with conversation history.

When you run `f ai list`, Flow:
1. Converts your current directory to Claude's project folder name
2. Reads all session files in that folder
3. Extracts timestamps and first messages for display
4. Cross-references with your saved sessions in `.ai/`

## Tips

1. **After a productive session**: Run `f ai save meaningful-name` to bookmark it
2. **Starting on a new machine**: Run `f ai import` to populate saved sessions
3. **Picking up where you left off**: `f ai resume` continues the most recent session
4. **Documenting decisions**: Use `f ai notes` to capture context for future you
