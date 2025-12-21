//! Auto-setup command for autonomous agent workflows.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::config;

/// Generate agents.md content with project-specific settings.
fn generate_agents_md(project_name: &str, primary_task: &str) -> String {
    format!(r#"# Autonomous Agent Instructions

Project: {project_name}
Primary task: {primary_task}

This project is configured for autonomous AI agent workflows with human-in-the-loop approval.

## Response Format

**Every response MUST end with one of these signals:**

### `runFlowTask: {primary_task} (.)`
Use after implementing code changes. This is the PRIMARY signal - most responses should end with this.

Examples:
```
runFlowTask: {primary_task} (.)
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

## Critical Rule: Implementation → {primary_task}

**After implementing ANY code change, ALWAYS end with:**
```
runFlowTask: {primary_task} (.)
```

This ensures the code gets built and deployed. The human will approve via the widget.

## Flow Priority

1. **Code change made** → `runFlowTask: {primary_task} (.)`
2. **Tests needed** → `runFlowTask: test (.)`
3. **Ready to commit** → `runFlowTask: commitWithCheck (.)`
4. **Blocked/need input** → `notify: <reason>`

## Examples

### After implementing a feature
```
Done. Added the new command.

runFlowTask: {primary_task} (.)
```

### After fixing a bug
```
Fixed the null pointer exception.

runFlowTask: {primary_task} (.)
```

### When blocked
```
notify: Cannot implement - need database connection string
```

## Available Flow Tasks

Run `f tasks` to see all available tasks for this project.
"#)
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
        let name = cfg.project_name
            .or_else(|| cwd.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "project".to_string());
        let task = cfg.flow.primary_task.unwrap_or_else(|| "deploy".to_string());
        (name, task)
    } else {
        let name = cwd.file_name()
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

    fs::write(&agents_path, &agents_content)
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

    fs::write(&codex_agents_path, &agents_content)
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
    println!("  runFlowTask: {} (.)  - Deploy after code changes", primary_task);
    println!("  notify: <message>       - Tell something to human");
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
