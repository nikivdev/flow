//! Gen agents integration.
//!
//! Invokes gen AI agents from the flow CLI.
//! Gen is opencode with GEN_MODE=1, providing flow integration.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use shell_words;

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

const BUILTIN_SUBAGENTS: &[(&str, &str)] = &[
    ("flow", "Flow-aware agent with full context about flow.toml, tasks, and CLI"),
    ("explore", "Fast codebase exploration: find files, search code, answer questions"),
    ("codify", "Convert scripts/sessions to reusable TypeScript for Bun"),
    ("general", "Multi-step autonomous tasks, parallel execution"),
];

/// Run the agents subcommand.
pub fn run(cmd: AgentsCommand) -> Result<()> {
    match cmd.action {
        Some(AgentsAction::List) => list_agents(),
        Some(AgentsAction::Run { agent, prompt }) => run_agent(&agent, prompt),
        Some(AgentsAction::Global { agent, prompt }) => run_global_agent(&agent, prompt),
        None => {
            if cmd.agent.is_empty() {
                run_fuzzy_agents()
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
    println!("  f agents os-health clean           # Cleanup non-system offenders");
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

#[derive(Debug, Clone, Copy)]
enum AgentKind {
    Global,
    Subagent,
}

struct AgentEntry {
    name: String,
    display: String,
    kind: AgentKind,
}

struct FzfAgentResult<'a> {
    entry: &'a AgentEntry,
    with_args: bool,
}

fn run_fuzzy_agents() -> Result<()> {
    let entries = build_agent_entries()?;
    if entries.is_empty() {
        println!("No agents available.");
        return Ok(());
    }

    if which::which("fzf").is_err() {
        println!("fzf not found on PATH – install it to use fuzzy selection.");
        list_agents()?;
        return Ok(());
    }

    if let Some(result) = run_agent_fzf(&entries)? {
        let mut prompt_args = if result.with_args {
            prompt_for_agent_prompt(&result.entry.name)?
        } else {
            Vec::new()
        };

        match result.entry.kind {
            AgentKind::Global => {
                if prompt_args.is_empty() {
                    run_global_agent(&result.entry.name, None)?;
                } else {
                    run_global_agent(&result.entry.name, Some(prompt_args))?;
                }
            }
            AgentKind::Subagent => {
                if prompt_args.is_empty() {
                    prompt_args = prompt_for_agent_prompt(&result.entry.name)?;
                }
                if prompt_args.is_empty() {
                    bail!("No prompt provided.");
                }
                run_agent(&result.entry.name, prompt_args)?;
            }
        }
    }

    Ok(())
}

fn run_agent_fzf<'a>(entries: &'a [AgentEntry]) -> Result<Option<FzfAgentResult<'a>>> {
    let mut child = Command::new("fzf")
        .arg("--prompt")
        .arg("agents> ")
        .arg("--expect")
        .arg("tab")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to spawn fzf")?;

    {
        let stdin = child.stdin.as_mut().context("failed to open fzf stdin")?;
        for entry in entries {
            writeln!(stdin, "{}", entry.display)?;
        }
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let raw = String::from_utf8(output.stdout).context("fzf output was not valid UTF-8")?;
    let mut lines = raw.lines();

    let key = lines.next().unwrap_or("");
    let with_args = key == "tab";

    let selection = lines.next().unwrap_or("").trim();
    if selection.is_empty() {
        return Ok(None);
    }

    let entry = entries.iter().find(|entry| entry.display == selection);
    Ok(entry.map(|e| FzfAgentResult {
        entry: e,
        with_args,
    }))
}

fn prompt_for_agent_prompt(agent_name: &str) -> Result<Vec<String>> {
    use std::io::{self, BufRead};

    println!("(tip: use quotes for prompts with spaces, e.g. 'find all API endpoints')");
    print!("f agents {} ", agent_name);
    io::stdout().flush()?;

    let stdin = io::stdin();
    let line = stdin.lock().lines().next();
    let input = match line {
        Some(Ok(s)) => s,
        _ => return Ok(Vec::new()),
    };

    let args = shell_words::split(&input).context("failed to parse prompt")?;
    Ok(args)
}

fn build_agent_entries() -> Result<Vec<AgentEntry>> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();

    for (name, desc) in GLOBAL_AGENTS {
        let display = format!("[global] {} - {}", name, desc);
        if seen.insert(display.clone()) {
            entries.push(AgentEntry {
                name: (*name).to_string(),
                display,
                kind: AgentKind::Global,
            });
        }
    }

    for (name, desc) in BUILTIN_SUBAGENTS {
        let display = format!("[builtin] {} - {}", name, desc);
        if seen.insert(display.clone()) {
            entries.push(AgentEntry {
                name: (*name).to_string(),
                display,
                kind: AgentKind::Subagent,
            });
        }
    }

    if let Some(project_root) = find_project_root() {
        let opencode_dir = project_root.join(".opencode");
        entries.extend(collect_agent_entries(
            &opencode_dir.join("agent"),
            "project",
            &mut seen,
        )?);
        entries.extend(collect_agent_entries(
            &opencode_dir.join("agents"),
            "project",
            &mut seen,
        )?);
    }

    if let Some(global_dir) = dirs::config_dir().map(|d| d.join("opencode")) {
        entries.extend(collect_agent_entries(
            &global_dir.join("agent"),
            "global-config",
            &mut seen,
        )?);
        entries.extend(collect_agent_entries(
            &global_dir.join("agents"),
            "global-config",
            &mut seen,
        )?);
    }

    Ok(entries)
}

fn find_project_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join(".opencode").exists() || dir.join("flow.toml").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

fn collect_agent_entries(
    root: &Path,
    label: &str,
    seen: &mut HashSet<String>,
) -> Result<Vec<AgentEntry>> {
    let mut entries = Vec::new();
    if !root.exists() {
        return Ok(entries);
    }

    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(false)
        .git_exclude(false)
        .ignore(false)
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }

        let name = match agent_name_from_path(root, path) {
            Some(n) => n,
            None => continue,
        };
        let (desc, mode) = parse_agent_frontmatter(path)?;
        if matches!(mode.as_deref(), Some("primary")) {
            continue;
        }
        let summary = desc.unwrap_or_else(|| "No description".to_string());
        let display = format!("[{label}] {} - {}", name, summary);
        if seen.insert(display.clone()) {
            entries.push(AgentEntry {
                name,
                display,
                kind: AgentKind::Subagent,
            });
        }
    }

    Ok(entries)
}

fn agent_name_from_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let without_ext = relative.with_extension("");
    Some(without_ext.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
}

fn parse_agent_frontmatter(path: &Path) -> Result<(Option<String>, Option<String>)> {
    let contents = fs::read_to_string(path).unwrap_or_default();
    let mut lines = contents.lines();
    if lines.next().map(|l| l.trim()) != Some("---") {
        return Ok((None, None));
    }

    let mut desc: Option<String> = None;
    let mut mode: Option<String> = None;
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        if let Some(value) = line.strip_prefix("description:") {
            desc = Some(trim_yaml_scalar(value));
        } else if let Some(value) = line.strip_prefix("mode:") {
            mode = Some(trim_yaml_scalar(value));
        }
    }

    Ok((desc, mode))
}

fn trim_yaml_scalar(value: &str) -> String {
    let trimmed = value.trim();
    trimmed
        .trim_matches('"')
        .trim_matches('\'')
        .to_string()
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
    let full_prompt = match prompt {
        Some(p) if !p.is_empty() => {
            if agent == "os-health" && is_os_health_clean_request(&p) {
                build_os_health_clean_prompt()
            } else {
                format!(
                    "Use the Task tool with subagent_type='{}' to: {}",
                    agent,
                    p.join(" ")
                )
            }
        }
        _ => build_global_agent_prompt(agent),
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
             Identify high-CPU or hanging processes on macOS and report offenders. \
             Then ask: \"Proceed with cleanup? (y/n)\". \
             If the user answers \"y\", terminate non-system offenders with TERM, \
             recheck, and only use KILL if needed. \
             Do not terminate system processes; report them and ask before any action."
        ),
        _ => format!("Use the Task tool with subagent_type='{}' to complete the task.", agent),
    }
}

fn is_os_health_clean_request(prompt: &[String]) -> bool {
    if prompt.len() != 1 {
        return false;
    }
    matches!(
        prompt[0].to_lowercase().as_str(),
        "clean" | "cleanup"
    )
}

fn build_os_health_clean_prompt() -> String {
    "Use the Task tool with subagent_type='os-health' to: \
     Identify high-CPU or hanging processes on macOS and report offenders. \
     Ask \"Proceed with cleanup? (y/n)\" and if the user answers \"y\", \
     terminate non-system offenders with TERM, recheck, and only use KILL if needed. \
     Do not terminate system processes; report them and ask before any action."
        .to_string()
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
