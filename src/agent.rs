//! Kode subagent integration.
//!
//! Invokes kode AI subagents from the flow CLI.
//! Kode is opencode with KODE_MODE=1, providing flow integration.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::cli::{AgentAction, AgentCommand};
use crate::discover;

/// Default kode repository location.
const KODE_REPO: &str = "/Users/nikiv/org/1f/kode";

/// Run the agent subcommand.
pub fn run(cmd: AgentCommand) -> Result<()> {
    let action = cmd.action.unwrap_or(AgentAction::List);

    match action {
        AgentAction::List => list_agents(),
        AgentAction::Run { agent, prompt } => run_agent(&agent, prompt),
    }
}

/// Find kode - either the installed binary or the repo.
fn find_kode() -> Option<KodeLocation> {
    // Check ~/.local/bin/kode first (installed via `f link` in kode repo)
    if let Some(home) = dirs::home_dir() {
        let local_bin = home.join(".local/bin/kode");
        if local_bin.exists() {
            return Some(KodeLocation::Binary(local_bin));
        }
    }

    // Check PATH
    if let Ok(path) = which::which("kode") {
        return Some(KodeLocation::Binary(path));
    }

    // Fall back to repo location
    let repo = PathBuf::from(KODE_REPO);
    if repo.join("packages/opencode/src/index.ts").exists() {
        return Some(KodeLocation::Repo(repo));
    }

    // Check KODE_REPO env var
    if let Ok(env_repo) = std::env::var("KODE_REPO") {
        let repo = PathBuf::from(&env_repo);
        if repo.join("packages/opencode/src/index.ts").exists() {
            return Some(KodeLocation::Repo(repo));
        }
    }

    None
}

enum KodeLocation {
    Binary(PathBuf),
    Repo(PathBuf),
}

/// List available agents.
fn list_agents() -> Result<()> {
    println!("Flow agents:\n");
    println!("  flow     - Flow-aware agent with full context about flow.toml, tasks, and CLI");
    println!("             Knows schema, best practices, and can create/modify tasks");
    println!();
    println!("Subagents (via Task tool):\n");
    println!("  explore  - Fast codebase exploration: find files, search code, answer questions");
    println!("             Thoroughness: quick, medium, very thorough");
    println!("  codify   - Convert scripts/sessions to reusable TypeScript for Bun");
    println!("  general  - Multi-step autonomous tasks, parallel execution");
    println!();
    println!("Primary agents (standalone modes):\n");
    println!("  build    - Default coding/building agent (full permissions)");
    println!("  plan     - Planning mode with read-only restrictions");
    println!();
    println!("Usage:");
    println!("  f a run flow \"add a deploy task that builds and pushes to docker\"");
    println!("  f a run explore \"find all API endpoints\"");
    println!("  f a run codify \"convert shell scripts to TypeScript\"");
    println!("  f a run general \"refactor auth module and update tests\"");
    println!();

    match find_kode() {
        Some(KodeLocation::Binary(p)) => println!("Using: {}", p.display()),
        Some(KodeLocation::Repo(p)) => println!("Using repo: {}", p.display()),
        None => {
            println!("⚠ kode not found. Install with:");
            println!("  cd {} && f link", KODE_REPO);
            println!("  # or set KODE_REPO environment variable");
        }
    }

    Ok(())
}

/// Run an agent with a prompt.
fn run_agent(agent: &str, prompt: Vec<String>) -> Result<()> {
    let kode = find_kode().ok_or_else(|| {
        anyhow::anyhow!(
            "kode not found. Install with:\n  cd {} && f link\n  # or set KODE_REPO env var",
            KODE_REPO
        )
    })?;

    let prompt_str = prompt.join(" ");
    if prompt_str.is_empty() {
        bail!(
            "No prompt provided.\nUsage: f agent run {} \"your prompt here\"",
            agent
        );
    }

    // Build the full prompt based on agent type
    let full_prompt = if agent == "flow" {
        build_flow_prompt(&prompt_str)?
    } else {
        // Regular subagent - use Task tool
        format!(
            "Use the Task tool with subagent_type='{}' to: {}",
            agent, prompt_str
        )
    };

    println!("Invoking {} agent...\n", agent);

    let status = match kode {
        KodeLocation::Binary(path) => Command::new(&path)
            .args(["run", &full_prompt])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("failed to run kode")?,

        KodeLocation::Repo(repo) => Command::new("bun")
            .args([
                "run",
                "--cwd",
                &repo.join("packages/opencode").to_string_lossy(),
                "--conditions=browser",
                "src/index.ts",
                "run",
                &full_prompt,
            ])
            .env("KODE_MODE", "1")
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("failed to run kode from repo")?,
    };

    if !status.success() {
        bail!("Agent exited with status: {}", status);
    }

    Ok(())
}

/// Build a flow-aware prompt with full context.
fn build_flow_prompt(user_prompt: &str) -> Result<String> {
    let mut context = String::new();

    // Add flow.toml schema reference
    context.push_str(FLOW_SCHEMA_CONTEXT);

    // Add current project tasks if available
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Ok(discovery) = discover::discover_tasks(&cwd) {
        if !discovery.tasks.is_empty() {
            context.push_str("\n\n## Current Project Tasks\n\n");
            for task in &discovery.tasks {
                let desc = task.task.description.as_deref().unwrap_or("");
                if task.relative_dir.is_empty() {
                    context.push_str(&format!("- `{}`: {} ({})\n", task.task.name, desc, task.task.command));
                } else {
                    context.push_str(&format!("- `{}` ({}): {} ({})\n", task.task.name, task.relative_dir, desc, task.task.command));
                }
            }
        }
    }

    // Add CLI commands reference
    context.push_str(FLOW_CLI_CONTEXT);

    Ok(format!(
        "{}\n\n---\n\nUser request: {}\n\nComplete this task. Read flow.toml first if you need to modify it.",
        context, user_prompt
    ))
}

/// Flow.toml schema and best practices context.
const FLOW_SCHEMA_CONTEXT: &str = r#"# Flow Task Runner Context

You are a flow-aware agent. Flow is a task runner with these key concepts:

## flow.toml Schema

```toml
version = 1
name = "project-name"  # optional project identifier

[flow]
primary_task = "dev"  # default task for `f` with no args

[[tasks]]
name = "task-name"           # required: unique identifier
command = "echo hello"       # required: shell command to run
description = "What it does" # optional: shown in task list
shortcuts = ["t", "tn"]      # optional: short aliases
dependencies = ["other-task", "cargo"]  # optional: run before, or ensure binary exists
interactive = false          # optional: needs TTY (auto-detected for sudo, vim, etc.)
delegate_to_hub = false      # optional: run via background hub daemon
on_cancel = "cleanup cmd"    # optional: run when Ctrl+C pressed

# Dependencies section - define reusable deps
[deps]
cargo = "cargo"              # simple binary check
node = ["node", "npm"]       # multiple binaries
ripgrep = { pkg-path = "ripgrep" }  # flox managed package

# Flox integration for reproducible dependencies
[flox.install]
cargo.pkg-path = "cargo"
nodejs.pkg-path = "nodejs"
```

## Best Practices

1. **Task naming**: Use kebab-case (e.g., `deploy-prod`, `test-unit`)
2. **Shortcuts**: Add 1-3 char shortcuts for frequent tasks
3. **Descriptions**: Always add descriptions - they appear in `f tasks` and fuzzy search
4. **Dependencies**: List task deps (run first) or binary deps (check PATH)
5. **on_cancel**: Add cleanup for long-running tasks that spawn processes
6. **interactive**: Auto-detected for sudo/vim/ssh, set manually for custom TUIs
"#;

/// Flow CLI commands context.
const FLOW_CLI_CONTEXT: &str = r#"

## Flow CLI Commands

- `f` - Fuzzy search tasks (fzf picker)
- `f <task>` - Run task directly
- `f tasks` - List all tasks
- `f run <task> [args]` - Run task with args
- `f init` - Create flow.toml scaffold
- `f start` - Bootstrap .ai/ folder structure
- `f commit` - AI-assisted git commit
- `f agent run <type> "prompt"` - Run AI agent
- `f ps` - Show running tasks
- `f kill` - Stop running tasks
- `f logs <task>` - View task logs

## Task Arguments

Tasks receive args as positional params:
```toml
[[tasks]]
name = "greet"
command = "echo Hello $1"
```
Run: `f greet World` → prints "Hello World"
"#;
