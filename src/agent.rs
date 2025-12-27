//! Kode subagent integration.
//!
//! Invokes kode AI subagents from the flow CLI.
//! Kode is opencode with KODE_MODE=1, providing flow integration.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::cli::{AgentAction, AgentCommand};

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

    // Build the full prompt that tells kode to use the specific subagent
    let full_prompt = format!(
        "Use the Task tool with subagent_type='{}' to: {}",
        agent, prompt_str
    );

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
