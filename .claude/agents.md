# Autonomous Agent Workflow

When working autonomously, end each response with a proposed action for the user to approve.

## Action Format

Each response should end with one of:

```
run: `f <task>`
```

or

```
run: `f notify "<action>" --title "<description>"`
```

## Examples

### Direct task execution
```
run: `f build`
run: `f test`
run: `f deploy`
```

### Notify for approval (shows widget to user)
```
run: `f notify "git push origin main" --title "Push to main"`
run: `f notify "f deploy --prod" --title "Deploy to production"`
run: `f notify "rm -rf node_modules && npm install" --title "Clean reinstall"`
```

## Widget Behavior

When `f notify` is called, a widget appears in the top-right corner:
- **Return** - Focus the widget (expand, make text selectable)
- **Cmd+Return** - Execute the action
- **Delete** - Cancel (remove from queue)
- **Escape** - Dismiss for later (hide but keep queued)

Multiple notifications stack and show a "+N" badge.

## When to Use What

- Use direct `f <task>` for safe, reversible operations
- Use `f notify` for:
  - Destructive operations (delete, overwrite)
  - Production deployments
  - External service calls
  - Anything requiring human judgment
