# Autonomous Agent Instructions

This project is configured for autonomous AI agent workflows with human-in-the-loop approval.

## Response Format

**Every response MUST end with one of these signals:**

### `runFlowTask: <task> (<project-path>)`
Use after implementing code changes. This is the PRIMARY signal - most responses should end with this.
Include the project path so the task runs in the correct directory.

Examples:
```
runFlowTask: deploy (cli/flow)
runFlowTask: build (packages/web)
runFlowTask: test (src/api)
runFlowTask: commitWithCheck (.)
```

### `notify: <message>`
Use ONLY when you cannot proceed or need human input. This pauses the autonomous flow.

Examples:
```
notify: Need clarification on the database schema
notify: Cannot proceed - missing API key
notify: Build failed - requires manual fix
```

## Critical Rule: Implementation → Deploy

**After implementing ANY code change, ALWAYS end with:**
```
runFlowTask: deploy (<project-path>)
```

This ensures the code gets built and deployed. The human will approve via the widget.

## Flow Priority

1. **Code change made** → `runFlowTask: deploy (<path>)`
2. **Tests needed** → `runFlowTask: test (<path>)`
3. **Ready to commit** → `runFlowTask: commitWithCheck (<path>)`
4. **Blocked/need input** → `notify: <reason>`

## Examples

### After implementing a feature
```
Done. Added the zed-focus-from-warp command.

runFlowTask: deploy (cli/flow)
```

### After fixing a bug
```
Fixed the null pointer exception in user service.

runFlowTask: deploy (packages/api)
```

### After refactoring
```
Refactored authentication to use JWT.

runFlowTask: test (src/auth)
```

### When blocked
```
notify: Cannot implement - need database connection string
```

## Available Flow Tasks

- `deploy` - Build and deploy the project
- `build` - Build only
- `test` - Run tests
- `commit` - AI-powered commit
- `commitWithCheck` - Commit with code review
