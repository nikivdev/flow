# Testing Pi-Mono Extensibility

This directory contains a test extension demonstrating pi-mono's extensibility.

## Setup

The extension is at `.pi/extensions/test-extensibility.ts` and loads automatically.

## Run

```bash
cd ~/code/flow
pi
```

## Test Commands

Once pi is running, try these:

### 1. Test Custom Tool

```
use the counter tool to increment by 5
```

```
increment the counter 3 times, then show me the value
```

### 2. Test Event Hooks

```
run: echo "hello world"
```

(Watch the console for `[test-ext] Tool called: bash`)

```
run: rm -i test.txt
```

(Should show a warning notification)

### 3. Test Custom Command

Type directly:
```
/count
```

### 4. View Extension Logs

The extension logs to console. Look for `[test-ext]` prefixed messages.

## What This Demonstrates

1. **Custom Tools** - `counter` tool with multiple actions
2. **Event Hooks** - `tool_call` and `turn_end` listeners
3. **Custom Commands** - `/count` slash command
4. **Session Events** - Reset state on `session_start`
5. **UI Interactions** - `ctx.ui.notify()` for warnings

## Extending Further

Edit `.pi/extensions/test-extensibility.ts` to:

- Add more tools
- Block dangerous operations (return `{ block: true }`)
- Add keyboard shortcuts with `pi.registerShortcut()`
- Register custom LLM providers with `pi.registerProvider()`
