//! Auto-setup command for autonomous agent workflows.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::config;

/// Generate agents.md content with project-specific settings.
fn generate_agents_md(project_name: &str, _primary_task: &str) -> String {
    format!(
        r#"# Autonomous Agent Instructions

Project: {project_name}

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
"#
    )
}

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

    let cwd = std::env::current_dir().context("failed to get current directory")?;

    // Load flow.toml to get project settings
    let flow_toml = cwd.join("flow.toml");
    let (project_name, primary_task) = if flow_toml.exists() {
        let cfg = config::load(&flow_toml).unwrap_or_default();
        let name = cfg
            .project_name
            .or_else(|| cwd.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "project".to_string());
        let task = cfg
            .flow
            .primary_task
            .unwrap_or_else(|| "deploy".to_string());
        (name, task)
    } else {
        let name = cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".to_string());
        (name, "deploy".to_string())
    };

    print!("Project: {} ", project_name);
    println!("(primary task: {})", primary_task);

    // Generate customized agents.md
    let agents_content = generate_agents_md(&project_name, &primary_task);

    // Create .claude directory if needed
    let claude_dir = cwd.join(".claude");
    fs::create_dir_all(&claude_dir).context("failed to create .claude directory")?;

    // Write agents.md
    let agents_path = claude_dir.join("agents.md");
    let existed = agents_path.exists();

    fs::write(&agents_path, &agents_content).context("failed to write agents.md")?;

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

    fs::write(&codex_agents_path, &agents_content).context("failed to write .codex/agents.md")?;

    if codex_existed {
        println!("Updated .codex/agents.md ✓");
    } else {
        println!("Created .codex/agents.md ✓");
    }

    println!();
    println!("Autonomous agent workflow is ready!");
    println!();
    println!("Claude Code and Codex will now end responses with:");
    println!("  done.              - Task completed successfully");
    println!("  done: <msg>        - Completed with context");
    println!("  needsUpdate: <msg> - Needs human decision");
    println!("  error: <msg>       - Task failed");
    println!();
    println!("Lin.app will detect these signals and show appropriate widgets.");

    Ok(())
}

/// Check if Lin.app is running.
fn is_lin_running() -> bool {
    let output = Command::new("pgrep").args(["-x", "Lin"]).output();

    match output {
        Ok(out) => out.status.success(),
        Err(_) => false,
    }
}
