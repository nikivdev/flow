//! Gen agents integration.
//!
//! Invokes gen AI agents from the flow CLI.
//! Gen is opencode with GEN_MODE=1, providing flow integration.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::cli::{AgentsAction, AgentsCommand};
use crate::discover;

/// Default gen repository location.
const GEN_REPO: &str = "/Users/nikiv/org/gen/gen";

/// Global agents that can be invoked from anywhere.
const GLOBAL_AGENTS: &[(&str, &str)] = &[
    ("repos-health", "Ensure all ~/repos have proper upstream configuration"),
    ("repos-sync", "Sync all ~/repos with upstream, resolve conflicts"),
    ("os-health", "Identify high-CPU or hanging processes and clean safely"),
];

/// Run the agents subcommand.
pub fn run(cmd: AgentsCommand) -> Result<()> {
    match cmd.action {
        Some(AgentsAction::List) => list_agents(),
        Some(AgentsAction::Run { agent, prompt }) => run_agent(&agent, prompt),
        Some(AgentsAction::Global { agent, prompt }) => run_global_agent(&agent, prompt),
        None => {
            if cmd.agent.is_empty() {
                list_agents()
            } else {
                let agent = &cmd.agent[0];
                let prompt = if cmd.agent.len() > 1 {
                    Some(cmd.agent[1..].to_vec())
                } else {
                    None
                };
                run_global_agent(agent, prompt)
            }
        }
    }
}

/// Find gen - either the installed binary or the repo.
fn find_gen() -> Option<GenLocation> {
    // Check ~/.local/bin/gen first (installed via `f install` in gen repo)
    if let Some(home) = dirs::home_dir() {
        let local_bin = home.join(".local/bin/gen");
        if local_bin.exists() {
            return Some(GenLocation::Binary(local_bin));
        }
    }

    // Check PATH
    if let Ok(path) = which::which("gen") {
        return Some(GenLocation::Binary(path));
    }

    // Fall back to repo location
    let repo = PathBuf::from(GEN_REPO);
    if repo.join("packages/opencode/src/index.ts").exists() {
        return Some(GenLocation::Repo(repo));
    }

    // Check GEN_REPO env var
    if let Ok(env_repo) = std::env::var("GEN_REPO") {
        let repo = PathBuf::from(&env_repo);
        if repo.join("packages/opencode/src/index.ts").exists() {
            return Some(GenLocation::Repo(repo));
        }
    }

    None
}

enum GenLocation {
    Binary(PathBuf),
    Repo(PathBuf),
}

/// List available agents.
fn list_agents() -> Result<()> {
    println!("Global agents:\n");
    for (name, desc) in GLOBAL_AGENTS {
        println!("  {:<14} - {}", name, desc);
    }
    println!();

    println!("Flow agents:\n");
    println!("  flow          - Flow-aware agent with full context about flow.toml, tasks, and CLI");
    println!("                  Knows schema, best practices, and can create/modify tasks");
    println!();

    println!("Subagents (via Task tool):\n");
    println!("  explore       - Fast codebase exploration: find files, search code, answer questions");
    println!("  codify        - Convert scripts/sessions to reusable TypeScript for Bun");
    println!("  general       - Multi-step autonomous tasks, parallel execution");
    println!();

    println!("Primary agents (standalone modes):\n");
    println!("  build         - Default coding/building agent (full permissions)");
    println!("  plan          - Planning mode with read-only restrictions");
    println!();

    println!("Usage:");
    println!("  f agents global repos-health       # Check all ~/repos have upstream");
    println!("  f agents global repos-sync         # Sync all ~/repos with upstream");
    println!("  f agents global os-health          # Identify high-CPU or hanging processes");
    println!("  f agents run flow \"add a deploy task\"");
    println!("  f agents run explore \"find all API endpoints\"");
    println!("  f agents os-health                 # Shortcut for global agents");
    println!();

    match find_gen() {
        Some(GenLocation::Binary(p)) => println!("Using: {}", p.display()),
        Some(GenLocation::Repo(p)) => println!("Using repo: {}", p.display()),
        None => {
            println!("⚠ gen not found. Install with:");
            println!("  cd {} && f install", GEN_REPO);
            println!("  # or set GEN_REPO environment variable");
        }
    }

    Ok(())
}

/// Run a global agent (repos-health, repos-sync, etc.)
fn run_global_agent(agent: &str, prompt: Option<Vec<String>>) -> Result<()> {
    // Validate it's a known global agent
    let agent_desc = GLOBAL_AGENTS
        .iter()
        .find(|(name, _)| *name == agent)
        .map(|(_, desc)| *desc);

    if agent_desc.is_none() {
        let available: Vec<_> = GLOBAL_AGENTS.iter().map(|(n, _)| *n).collect();
        bail!(
            "Unknown global agent: '{}'\nAvailable: {}",
            agent,
            available.join(", ")
        );
    }

    let gen_loc = find_gen().ok_or_else(|| {
        anyhow::anyhow!(
            "gen not found. Install with:\n  cd {} && f install\n  # or set GEN_REPO env var",
            GEN_REPO
        )
    })?;

    // Build prompt - use custom if provided, otherwise default for the agent
    let full_prompt = if let Some(p) = prompt {
        if p.is_empty() {
            build_global_agent_prompt(agent)
        } else {
            format!(
                "Use the Task tool with subagent_type='{}' to: {}",
                agent,
                p.join(" ")
            )
        }
    } else {
        build_global_agent_prompt(agent)
    };

    println!("Invoking {} agent...\n", agent);

    let status = invoke_gen(&gen_loc, &full_prompt)?;

    if !status.success() {
        bail!("Agent exited with status: {}", status);
    }

    Ok(())
}

/// Build default prompt for global agents.
fn build_global_agent_prompt(agent: &str) -> String {
    match agent {
        "repos-health" => format!(
            "Use the Task tool with subagent_type='repos-health' to: \
             Check all repositories in ~/repos and ensure they have proper upstream configuration. \
             For repos missing upstream, run 'f upstream setup --url <origin-url>'. \
             Report health status for each repo."
        ),
        "repos-sync" => format!(
            "Use the Task tool with subagent_type='repos-sync' to: \
             Sync all repositories in ~/repos with their upstream remotes. \
             For each repo, run 'f upstream sync' and resolve any merge conflicts \
             by preserving features from both sides. Report progress for each repo."
        ),
        "os-health" => format!(
            "Use the Task tool with subagent_type='os-health' to: \
             Identify high-CPU or hanging processes on macOS. \
             Report offenders and ask before terminating any process."
        ),
        _ => format!("Use the Task tool with subagent_type='{}' to complete the task.", agent),
    }
}

/// Run an agent with a prompt.
fn run_agent(agent: &str, prompt: Vec<String>) -> Result<()> {
    let gen_loc = find_gen().ok_or_else(|| {
        anyhow::anyhow!(
            "gen not found. Install with:\n  cd {} && f install\n  # or set GEN_REPO env var",
            GEN_REPO
        )
    })?;

    let prompt_str = prompt.join(" ");
    if prompt_str.is_empty() {
        bail!(
            "No prompt provided.\nUsage: f agents run {} \"your prompt here\"",
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

    let status = invoke_gen(&gen_loc, &full_prompt)?;

    if !status.success() {
        bail!("Agent exited with status: {}", status);
    }

    Ok(())
}

/// Invoke gen with a prompt.
fn invoke_gen(location: &GenLocation, prompt: &str) -> Result<std::process::ExitStatus> {
    match location {
        GenLocation::Binary(path) => Command::new(path)
            .args(["run", prompt])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("failed to run gen"),

        GenLocation::Repo(repo) => Command::new("bun")
            .args([
                "run",
                "--cwd",
                &repo.join("packages/opencode").to_string_lossy(),
                "--conditions=browser",
                "src/index.ts",
                "run",
                prompt,
            ])
            .env("GEN_MODE", "1")
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("failed to run gen from repo"),
    }
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
- `f agents run <type> "prompt"` - Run AI agent
- `f agents global <agent>` - Run global agent
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
