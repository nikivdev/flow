# Autonomous Agent Instructions

Project: flow
Primary task: deploy-cli-release

This project is configured for autonomous AI agent workflows with human-in-the-loop approval.

## Response Format

**Every response MUST end with one of these signals:**

### `runFlowTask: deploy-cli-release (.)`
Use after implementing code changes. This is the PRIMARY signal - most responses should end with this.

Examples:
```
runFlowTask: deploy-cli-release (.)
runFlowTask: test (.)
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

## Critical Rule: Implementation → deploy-cli-release

**After implementing ANY code change, ALWAYS end with:**
```
runFlowTask: deploy-cli-release (.)
```

This ensures the code gets built and deployed. The human will approve via the widget.

## Flow Priority

1. **Code change made** → `runFlowTask: deploy-cli-release (.)`
2. **Tests needed** → `runFlowTask: test (.)`
3. **Ready to commit** → `runFlowTask: commitWithCheck (.)`
4. **Blocked/need input** → `notify: <reason>`

## Examples

### After implementing a feature
```
Done. Added the new command.

runFlowTask: deploy-cli-release (.)
```

### After fixing a bug
```
Fixed the null pointer exception.

runFlowTask: deploy-cli-release (.)
```

### When blocked
```
notify: Cannot implement - need database connection string
```

## Available Flow Tasks

Run `f tasks` to see all available tasks for this project.
