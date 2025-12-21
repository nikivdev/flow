//! Auto-setup command for autonomous agent workflows.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const AGENTS_MD_CONTENT: &str = r#"# Autonomous Agent Instructions

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
"#;

/// Run the auto-setup command.
pub fn run() -> Result<()> {
    println!("Setting up autonomous agent workflow...\n");

    // Check if Lin.app is running
    print!("Checking Lin.app... ");
    if !is_lin_running() {
        println!("not running");
        println!();
        println!("Lin.app is required for autonomous agent workflows.");
        println!("Please start Lin.app from /Applications/Lin.app");
        bail!("Lin.app is not running");
    }
    println!("running ✓");

    // Check if Lin.app exists
    let lin_app = PathBuf::from("/Applications/Lin.app");
    if !lin_app.exists() {
        println!();
        println!("Warning: Lin.app not found at /Applications/Lin.app");
        println!("The autonomous workflow requires Lin.app to be installed.");
    }

    // Create .claude directory if needed
    let cwd = std::env::current_dir().context("failed to get current directory")?;
    let claude_dir = cwd.join(".claude");
    fs::create_dir_all(&claude_dir).context("failed to create .claude directory")?;

    // Write agents.md
    let agents_path = claude_dir.join("agents.md");
    let existed = agents_path.exists();

    fs::write(&agents_path, AGENTS_MD_CONTENT)
        .context("failed to write agents.md")?;

    if existed {
        println!("Updated .claude/agents.md ✓");
    } else {
        println!("Created .claude/agents.md ✓");
    }

    // Also create for Codex (.codex/agents.md)
    let codex_dir = cwd.join(".codex");
    fs::create_dir_all(&codex_dir).context("failed to create .codex directory")?;

    let codex_agents_path = codex_dir.join("agents.md");
    let codex_existed = codex_agents_path.exists();

    fs::write(&codex_agents_path, AGENTS_MD_CONTENT)
        .context("failed to write .codex/agents.md")?;

    if codex_existed {
        println!("Updated .codex/agents.md ✓");
    } else {
        println!("Created .codex/agents.md ✓");
    }

    println!();
    println!("Autonomous agent workflow is ready!");
    println!();
    println!("Claude Code and Codex will now end responses with:");
    println!("  notify: <message>       - Tell something to human");
    println!("  runFlowTask: <task>     - Propose running a flow task");
    println!();
    println!("Lin.app will show widgets for approval when these signals are detected.");

    Ok(())
}

/// Check if Lin.app is running.
fn is_lin_running() -> bool {
    let output = Command::new("pgrep")
        .args(["-x", "Lin"])
        .output();

    match output {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}
