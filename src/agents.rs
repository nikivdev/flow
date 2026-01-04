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

const FLOW_AGENT_NAME: &str = "flow";

/// Run the agents subcommand.
pub fn run(cmd: AgentsCommand) -> Result<()> {
    match cmd.action {
        Some(AgentsAction::List) => list_agents(),
        Some(AgentsAction::Run { agent, prompt }) => run_agent(&agent, prompt),
        Some(AgentsAction::Global { agent, prompt }) => run_agent_optional(&agent, prompt),
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
                run_agent_optional(agent, prompt)
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
    println!("Flow agents:\n");
    println!("  flow          - Flow-aware agent with full context about flow.toml, tasks, and CLI");
    println!("                  Knows schema, best practices, and can create/modify tasks");
    println!();

    println!("Gen agents (project + global config):\n");
    if let Some(gen_loc) = find_gen() {
        if let Err(err) = list_gen_agents(&gen_loc) {
            println!("⚠ failed to list gen agents: {err}");
        }
    } else {
        println!("⚠ gen not found. Install with:");
        println!("  cd {} && f install", GEN_REPO);
        println!("  # or set GEN_REPO environment variable");
    }

    println!();
    println!("Usage:");
    println!("  f agents                    # Fuzzy search agents");
    println!("  f agents run <agent> \"prompt\"");
    println!("  f agents <agent>            # Run agent (prompts for input)");
    println!();

    Ok(())
}

struct AgentEntry {
    name: String,
    display: String,
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
        if prompt_args.is_empty() {
            prompt_args = prompt_for_agent_prompt(&result.entry.name)?;
        }
        if prompt_args.is_empty() {
            bail!("No prompt provided.");
        }
        run_agent(&result.entry.name, prompt_args)?;
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
    let flow_display = "[flow] flow - Flow-aware agent for flow.toml tasks and CLI";
    seen.insert(flow_display.to_string());
    entries.push(AgentEntry {
        name: FLOW_AGENT_NAME.to_string(),
        display: flow_display.to_string(),
    });

    if let Ok(gen_entries) = fetch_gen_agent_entries() {
        for entry in gen_entries {
            if seen.insert(entry.display.clone()) {
                entries.push(entry);
            }
        }
        return Ok(entries);
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

fn apply_project_config_env(cmd: &mut Command) {
    if let Some(root) = find_project_root() {
        let opencode_dir = root.join(".opencode");
        if opencode_dir.exists() {
            cmd.env("OPENCODE_CONFIG_DIR", opencode_dir);
        }
    }
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

fn run_agent_optional(agent: &str, prompt: Option<Vec<String>>) -> Result<()> {
    let mut prompt_args = match prompt {
        Some(p) if !p.is_empty() => p,
        _ => prompt_for_agent_prompt(agent)?,
    };

    if prompt_args.is_empty() {
        bail!("No prompt provided.");
    }

    run_agent(agent, prompt_args)
}

fn list_gen_agents(gen_loc: &GenLocation) -> Result<()> {
    let status = match gen_loc {
        GenLocation::Binary(path) => Command::new(path)
            .args(["agent", "list"])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .context("failed to run gen agent list")?,
        GenLocation::Repo(repo) => {
            let mut cmd = Command::new("bun");
            cmd.args([
                "run",
                "--cwd",
                &repo.join("packages/opencode").to_string_lossy(),
                "--conditions=browser",
                "src/index.ts",
                "agent",
                "list",
            ])
            .env("GEN_MODE", "1")
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
            apply_project_config_env(&mut cmd);
            cmd.status().context("failed to run gen agent list")?
        }
    };

    if status.success() {
        Ok(())
    } else {
        bail!("gen agent list failed");
    }
}

fn fetch_gen_agent_entries() -> Result<Vec<AgentEntry>> {
    let gen_loc = find_gen().ok_or_else(|| {
        anyhow::anyhow!(
            "gen not found. Install with:\n  cd {} && f install\n  # or set GEN_REPO env var",
            GEN_REPO
        )
    })?;

    let output = match gen_loc {
        GenLocation::Binary(ref path) => Command::new(path)
            .args(["agent", "list"])
            .output()
            .context("failed to run gen agent list")?,
        GenLocation::Repo(ref repo) => {
            let mut cmd = Command::new("bun");
            cmd.args([
                "run",
                "--cwd",
                &repo.join("packages/opencode").to_string_lossy(),
                "--conditions=browser",
                "src/index.ts",
                "agent",
                "list",
            ])
            .env("GEN_MODE", "1");
            apply_project_config_env(&mut cmd);
            cmd.output().context("failed to run gen agent list")?
        }
    };

    if !output.status.success() {
        bail!(
            "gen agent list failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_gen_agent_list(&stdout))
}

fn parse_gen_agent_list(stdout: &str) -> Vec<AgentEntry> {
    let mut entries = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((name, mode)) = parse_agent_list_line(line) {
            if mode == "primary" {
                continue;
            }
            let display = format!("[gen] {} ({})", name, mode);
            entries.push(AgentEntry { name, display });
        }
    }
    entries
}

fn parse_agent_list_line(line: &str) -> Option<(String, String)> {
    if line.starts_with('{')
        || line.starts_with('"')
        || line.starts_with('[')
        || line.starts_with("}")
    {
        return None;
    }
    let end = line.strip_suffix(')')?;
    let (name, mode) = end.rsplit_once(" (")?;
    if name.trim().is_empty() {
        return None;
    }
    let mode = mode.trim();
    if !matches!(mode, "subagent" | "all" | "primary") {
        return None;
    }
    Some((name.trim().to_string(), mode.to_string()))
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
    let full_prompt = if agent == FLOW_AGENT_NAME {
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

        GenLocation::Repo(repo) => {
            let mut cmd = Command::new("bun");
            cmd.args([
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
            .stderr(Stdio::inherit());
            apply_project_config_env(&mut cmd);
            cmd.status().context("failed to run gen from repo")
        }
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
