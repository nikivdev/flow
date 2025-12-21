# Autonomous Agent Instructions

This project is configured for autonomous AI agent workflows with human-in-the-loop approval.

## Response Format

**Every response must end with one of these signals:**

### `notify: <message>`
Use when you want to tell something to the human. This shows a widget for the user to acknowledge.

Examples:
```
notify: Task completed successfully
notify: Found 3 potential issues in the code
notify: Need clarification on the database schema
notify: Waiting for API key to be configured
```

### `runFlowTask: <task>`
Use when the user's original request seems incomplete and a follow-up flow task should run.
This shows a widget where the user can approve running the task.

Examples:
```
runFlowTask: f test
runFlowTask: f build
runFlowTask: f deploy
runFlowTask: f commitWithCheck
```

## Guidelines

1. **Always end with a signal** - Never leave a response without `notify:` or `runFlowTask:`
2. **Be specific** - Messages should clearly describe what happened or what's needed
3. **Chain tasks** - If you think another task should follow, use `runFlowTask:`
4. **Report issues** - Use `notify:` for errors, warnings, or anything requiring human attention

## Common Patterns

### After completing work
```
notify: Implemented the new feature as requested
```

### After completing work that needs testing
```
runFlowTask: f test
```

### After making changes that should be committed
```
runFlowTask: f commitWithCheck
```

### When encountering an error
```
notify: Build failed - missing dependency 'foo'
```

### When blocked
```
notify: Cannot proceed - need database credentials
```

## Available Flow Tasks

Run `f tasks` to see available tasks for this project.

Common tasks:
- `f build` - Build the project
- `f test` - Run tests
- `f commit` - AI-powered commit
- `f commitWithCheck` - Commit with Codex code review
- `f deploy` - Deploy (if configured)
