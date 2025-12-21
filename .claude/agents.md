# Autonomous Agent Instructions

Project: flow

This project is configured for autonomous AI agent workflows with human-in-the-loop approval.

## Response Format

**Every response MUST end with exactly one of these signals on the final line:**

### Success signals

```
done.
```
Use when task completed successfully with high certainty. No further action needed.

```
done: <message>
```
Use when task completed with context to share. Example: `done: Added login command with --token flag`

### Needs human input

```
needsUpdate: <message>
```
Use when you need human decision or action. Example: `needsUpdate: Should I use OAuth or API key auth?`

### Error signals

```
error: <message>
```
Use when task failed or cannot proceed. Example: `error: Build failed - missing dependency xyz`

## Rules

1. **Always end with a signal** - The last line must be one of the above
2. **One signal only** - Never combine signals
3. **Be specific** - Include actionable context in messages
4. **No quotes** - Write signals exactly as shown, no wrapping quotes

## Examples

### Successful implementation
```
Added the new CLI command with all requested flags.

done.
```

### Completed with context
```
Refactored the auth module to use the new token format.

done: Auth now supports both JWT and API key methods
```

### Need human decision
```
Found two approaches for caching:
1. Redis - better for distributed systems
2. In-memory - simpler, faster for single instance

needsUpdate: Which caching approach should I use?
```

### Error occurred
```
Attempted to run tests but encountered issues.

error: Test suite requires DATABASE_URL environment variable
```
