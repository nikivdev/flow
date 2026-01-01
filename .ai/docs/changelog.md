# Changelog

Auto-maintained changelog tracking flow features and changes.

## 2024-12-31

### Added
- **Documentation system** (`f docs`): Auto-generated documentation in `.ai/docs/`. Commands: `list`, `status`, `sync`, `edit`. Docs are updated by AI as part of commit flow.
- **Parallel task runner** (`f parallel`): Run multiple tasks concurrently with animated spinners, real-time status display, and pretty output. Supports custom labels (`label:command`) and fail-fast mode.
- **Docs update reminder**: Commit flow now detects when docs may need updating and shows a reminder.

### Changed
- **`.flox/` materialization**: Flox environment now gitignored and materialized from `.ai/flox/manifest.toml` via `f start`. Source of truth moved to `.ai/flox/`.
- **Gitignore section**: `f start` now adds `.flox/` to the `# flow` gitignore section.

## 2024-12-30

### Added
- **Deploy health check** (`f deploy health`): HTTP health check for deployments with configurable URL and expected status code.
- **GitEdit review sync**: `f commitWithCheckWithGitedit` now syncs diff, review results, and AI session data to gitedit.dev.

### Changed
- **`.ai/` folder restructure**: Separated public (tracked) and private (gitignored) content:
  - `.ai/actions/`, `.ai/skills/`, `.ai/tools/`, `.ai/flox/` - tracked
  - `.ai/internal/` - gitignored (sessions, checkpoints, db)
- **Tool folder materialization**: `.claude/` and `.codex/` now gitignored and materialized from `.ai/` via symlinks in `f start`.
- **Review instructions**: Now auto-discovered from `.ai/review.md`, `.ai/commit-review.md`, or `.ai/instructions.md`.
- **Fixers**: Pre-commit fixers now check first before processing, support scripts in `.ai/actions/`.

---

## Document Sync

This changelog is updated when commits add new features or make significant changes. To update:

1. Run `f docs sync` (when implemented)
2. Or manually add entries following the format above

### Tracking Commits

Recent commits that may need documentation:
- Check `git log --oneline -20` for recent changes
- Focus on user-facing features and behavior changes
