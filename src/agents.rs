//! Gen agents integration.
//!
//! Invokes gen AI agents from the flow CLI.
//! Gen is opencode with GEN_MODE=1, providing flow integration.

use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use ignore::WalkBuilder;
use serde_json::Value;
use shell_words;

use crate::cli::{AgentsAction, AgentsCommand};
use crate::config;
use crate::discover;

/// Default gen repository relative path under home dir.
const DEFAULT_GEN_REPO_REL: &str = "org/gen/gen";

const FLOW_AGENT_NAME: &str = "flow";

/// Run the agents subcommand.
pub fn run(cmd: AgentsCommand) -> Result<()> {
    match cmd.action {
        Some(AgentsAction::List) => list_agents(),
        Some(AgentsAction::Run { agent, prompt }) => run_agent(&agent, prompt),
        Some(AgentsAction::Global { agent, prompt }) => run_agent_optional(&agent, prompt),
        Some(AgentsAction::Copy { agent }) => copy_agent_instructions(agent.as_deref()),
        Some(AgentsAction::Rules { profile, repo }) => {
            run_agents_rules(profile.as_deref(), repo.as_deref())
        }
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

    // Check GEN_REPO env var
    if let Ok(env_repo) = std::env::var("GEN_REPO") {
        let repo = PathBuf::from(&env_repo);
        if repo.join("packages/opencode/src/index.ts").exists() {
            return Some(GenLocation::Repo(repo));
        }
    }

    // Fall back to repo location under home dir
    if let Some(repo) = default_gen_repo() {
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
    println!(
        "  flow          - Flow-aware agent with full context about flow.toml, tasks, and CLI"
    );
    println!("                  Knows schema, best practices, and can create/modify tasks");
    println!();

    println!("Gen agents (project + global config):\n");
    if let Some(gen_loc) = find_gen() {
        if let Err(err) = list_gen_agents(&gen_loc) {
            println!("⚠ failed to list gen agents: {err}");
        }
    } else {
        let default_repo = default_gen_repo()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| format!("~/{}", DEFAULT_GEN_REPO_REL));
        println!("⚠ gen not found. Install with:");
        println!("  cd {} && f install", default_repo);
        println!("  # or set GEN_REPO environment variable");
    }

    println!();
    println!("Usage:");
    println!("  f agents                    # Fuzzy search agents");
    println!("  f agents run <agent> \"prompt\"");
    println!("  f agents <agent>            # Run agent (prompts for input)");
    println!("  f agents rules              # Fuzzy pick agents.md profile");
    println!("  f agents rules <profile>    # Activate profile");
    println!();

    Ok(())
}

fn run_agents_rules(profile: Option<&str>, repo: Option<&str>) -> Result<()> {
    let mut repo_path = repo
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let mut chosen_profile = profile.map(|value| value.to_string());
    if repo.is_none() {
        if let Some(candidate) = profile {
            let candidate_path = PathBuf::from(candidate);
            if candidate_path.is_dir() {
                repo_path = candidate_path;
                chosen_profile = None;
            }
        }
    }

    let agents_dir = repo_path.join("agents");
    if !agents_dir.is_dir() {
        println!("No agents/ directory in {}", repo_path.display());
        return Ok(());
    }

    let mut profiles = list_agents_profiles(&agents_dir)?;
    if profiles.is_empty() {
        println!("No profiles found in {}", agents_dir.display());
        return Ok(());
    }
    profiles.sort();

    if let Some(name) = &chosen_profile {
        if !profiles.iter().any(|p| p == name) {
            bail!("Missing profile: {}", name);
        }
    } else {
        chosen_profile = select_agents_profile(&profiles)?;
    }

    let Some(profile_name) = chosen_profile else {
        println!("No profile selected.");
        return Ok(());
    };

    let source_lower = agents_dir.join(format!("agents.{profile_name}.md"));
    let source_upper = agents_dir.join(format!("AGENTS.{profile_name}.md"));
    let source = if source_lower.is_file() {
        source_lower
    } else {
        source_upper
    };
    if !source.is_file() {
        bail!("Missing profile file: {}", source.display());
    }
    let target = repo_path.join("agents.md");
    fs::copy(&source, &target)
        .with_context(|| format!("failed to copy {} to {}", source.display(), target.display()))?;

    let default_path = agents_dir.join(".default");
    fs::write(&default_path, &profile_name)
        .with_context(|| format!("failed to write {}", default_path.display()))?;

    println!("Activated agents.md -> {}", source.display());
    println!("Default profile set to: {}", profile_name);
    Ok(())
}

fn list_agents_profiles(agents_dir: &Path) -> Result<Vec<String>> {
    let mut profiles = HashSet::new();
    for entry in fs::read_dir(agents_dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let trimmed = if file_name.starts_with("agents.") && file_name.ends_with(".md") {
            file_name
                .trim_start_matches("agents.")
                .trim_end_matches(".md")
        } else if file_name.starts_with("AGENTS.") && file_name.ends_with(".md") {
            file_name
                .trim_start_matches("AGENTS.")
                .trim_end_matches(".md")
        } else {
            continue;
        };
        if !trimmed.is_empty() {
            profiles.insert(trimmed.to_string());
        }
    }
    Ok(profiles.into_iter().collect())
}

fn select_agents_profile(profiles: &[String]) -> Result<Option<String>> {
    if which::which("fzf").is_err() {
        println!("fzf not found on PATH – install it to use fuzzy selection.");
        return prompt_agents_profile(profiles).map(Some);
    }

    let mut child = Command::new("fzf")
        .arg("--prompt")
        .arg("agents rules> ")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to spawn fzf")?;

    {
        let stdin = child.stdin.as_mut().context("failed to open fzf stdin")?;
        for profile in profiles {
            writeln!(stdin, "{}", profile)?;
        }
    }

    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let selection = String::from_utf8(output.stdout)
        .context("fzf output was not valid UTF-8")?
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if selection.is_empty() {
        Ok(None)
    } else {
        Ok(Some(selection))
    }
}

fn prompt_agents_profile(profiles: &[String]) -> Result<String> {
    println!("Select an agents profile:");
    for (index, profile) in profiles.iter().enumerate() {
        println!("  {}) {}", index + 1, profile);
    }
    print!("choice> ");
    io::stdout().flush()?;

    let stdin = io::stdin();
    let line = stdin.lock().lines().next();
    let input = match line {
        Some(Ok(value)) => value.trim().to_string(),
        _ => "".to_string(),
    };
    if input.is_empty() {
        bail!("No profile selected.");
    }
    let idx: usize = input.parse().context("invalid selection")?;
    if idx == 0 || idx > profiles.len() {
        bail!("Selection out of range.");
    }
    Ok(profiles[idx - 1].clone())
}

struct AgentEntry {
    name: String,
    display: String,
    path: Option<PathBuf>,
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
        let prompt_args = if result.with_args {
            prompt_for_agent_prompt(&result.entry.name)?
        } else if DIR_AGENTS.contains(&result.entry.name.as_str()) {
            // Directory-based agents use cwd by default
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string());
            vec![cwd]
        } else {
            prompt_for_agent_prompt(&result.entry.name)?
        };
        if prompt_args.is_empty() {
            bail!("No prompt provided.");
        }
        run_agent(&result.entry.name, prompt_args)?;
    }

    Ok(())
}

/// Copy agent instructions to clipboard.
fn copy_agent_instructions(agent_name: Option<&str>) -> Result<()> {
    let entries = build_agent_entries()?;
    if entries.is_empty() {
        println!("No agents available.");
        return Ok(());
    }

    let selected = if let Some(name) = agent_name {
        // Find agent by name
        entries
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| anyhow::anyhow!("Agent '{}' not found", name))?
    } else {
        // Fuzzy select
        if which::which("fzf").is_err() {
            bail!("fzf not found on PATH – install it to use fuzzy selection.");
        }
        match run_agent_fzf_simple(&entries)? {
            Some(entry) => entry,
            None => return Ok(()),
        }
    };

    // Get agent file content
    let content = get_agent_content(&selected.name, selected.path.as_deref())?;

    if std::env::var("FLOW_NO_CLIPBOARD").is_ok() || !std::io::stdin().is_terminal() {
        println!("Clipboard disabled; skipping copy.");
        return Ok(());
    }

    // Copy to clipboard using pbcopy (macOS) or xclip (Linux)
    let mut cmd = if cfg!(target_os = "macos") {
        Command::new("pbcopy")
    } else {
        let mut c = Command::new("xclip");
        c.args(["-selection", "clipboard"]);
        c
    };

    let mut child = cmd
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to run clipboard command")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(content.as_bytes())
            .context("failed to write to clipboard")?;
    }

    child.wait()?;
    println!(
        "Copied '{}' agent instructions to clipboard ({} bytes)",
        selected.name,
        content.len()
    );
    Ok(())
}

/// Run fzf and return selected entry (simplified, no args prompt).
fn run_agent_fzf_simple<'a>(entries: &'a [AgentEntry]) -> Result<Option<&'a AgentEntry>> {
    let mut child = Command::new("fzf")
        .arg("--prompt")
        .arg("copy agent> ")
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
    let selection = String::from_utf8(output.stdout)
        .context("fzf output was not valid UTF-8")?
        .trim()
        .to_string();

    if selection.is_empty() {
        return Ok(None);
    }

    Ok(entries.iter().find(|entry| entry.display == selection))
}

/// Get agent file content by name and optional path.
fn get_agent_content(name: &str, path: Option<&Path>) -> Result<String> {
    // If path is provided, read directly
    if let Some(p) = path {
        return fs::read_to_string(p)
            .context(format!("failed to read agent file: {}", p.display()));
    }

    // Special case: flow agent has built-in instructions
    if name == FLOW_AGENT_NAME {
        return Ok(get_flow_agent_instructions());
    }

    // Try to find agent in common locations
    let mut locations = vec![
        dirs::home_dir().map(|h| h.join(".config/opencode/agent")),
        dirs::home_dir().map(|h| h.join(".opencode/agent")),
    ];
    if let Some(repo) = gen_repo_from_env() {
        locations.push(Some(repo.join(".opencode/agent")));
    }
    if let Some(repo) = default_gen_repo() {
        locations.push(Some(repo.join(".opencode/agent")));
    }

    for loc in locations.into_iter().flatten() {
        let agent_path = loc.join(format!("{}.md", name));
        if agent_path.exists() {
            return fs::read_to_string(&agent_path).context(format!(
                "failed to read agent file: {}",
                agent_path.display()
            ));
        }
    }

    bail!("Could not find agent file for '{}'", name)
}

/// Get built-in flow agent instructions.
fn get_flow_agent_instructions() -> String {
    r#"You are a Flow-aware agent with full context about flow.toml, tasks, and the Flow CLI.

## Capabilities
- Read and modify flow.toml configuration
- Create, update, and run tasks
- Understand the flow.toml schema and best practices
- Help with CI/CD workflows, dependencies, and project setup

## Guidelines
- Always read the existing flow.toml before making changes
- Preserve existing configuration when adding new items
- Use appropriate task names and descriptions
- Follow TOML formatting conventions
- Enforce Flow env store usage for secrets/tokens (use `f env get` / `f env run`)
"#
    .to_string()
}

fn default_gen_repo() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(DEFAULT_GEN_REPO_REL))
}

fn gen_repo_from_env() -> Option<PathBuf> {
    std::env::var("GEN_REPO").ok().map(PathBuf::from)
}

fn gen_repo_hint() -> String {
    default_gen_repo()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| format!("~/{}", DEFAULT_GEN_REPO_REL))
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
        path: None, // flow agent is built-in, no file
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
                path: Some(path.to_path_buf()),
            });
        }
    }

    Ok(entries)
}

fn agent_name_from_path(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let without_ext = relative.with_extension("");
    Some(
        without_ext
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/"),
    )
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
    trimmed.trim_matches('"').trim_matches('\'').to_string()
}

/// Agents that operate on the current directory by default.
const DIR_AGENTS: &[&str] = &["docker-to-flox"];

fn run_agent_optional(agent: &str, prompt: Option<Vec<String>>) -> Result<()> {
    let prompt_args = match prompt {
        Some(p) if !p.is_empty() => p,
        _ => {
            // For directory-based agents, use cwd as default
            if DIR_AGENTS.contains(&agent) {
                let cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string());
                vec![cwd]
            } else {
                prompt_for_agent_prompt(agent)?
            }
        }
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
            gen_repo_hint()
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
            entries.push(AgentEntry {
                name,
                display,
                path: None, // gen agents - path resolved separately
            });
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

/// Get the configured agent tool and model.
fn get_agent_config() -> (String, Option<String>) {
    if let Some(ts_config) = config::load_ts_config() {
        if let Some(flow) = ts_config.flow {
            if let Some(agents) = flow.agents {
                let tool = agents.tool.unwrap_or_else(|| "gen".to_string());
                return (tool, agents.model);
            }
        }
    }
    ("gen".to_string(), None)
}

/// Run an agent with a prompt.
fn run_agent(agent: &str, prompt: Vec<String>) -> Result<()> {
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

    let (tool, model) = get_agent_config();
    let status = match tool.as_str() {
        "claude" => invoke_claude(&full_prompt)?,
        "opencode" => invoke_opencode(&full_prompt, model.as_deref())?,
        _ => {
            let gen_loc = find_gen().ok_or_else(|| {
                anyhow::anyhow!(
                    "gen not found. Install with:\n  cd {} && f install\n  # or set GEN_REPO env var",
                    gen_repo_hint()
                )
            })?;
            invoke_gen(&gen_loc, &full_prompt)?
        }
    };

    if !status.success() {
        bail!("Agent exited with status: {}", status);
    }

    Ok(())
}

/// Invoke Claude Code with a prompt.
fn invoke_claude(prompt: &str) -> Result<std::process::ExitStatus> {
    Command::new("claude")
        .args(["-p", prompt])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run claude")
}

/// Invoke opencode with a prompt and optional model.
fn invoke_opencode(prompt: &str, model: Option<&str>) -> Result<std::process::ExitStatus> {
    let mut cmd = Command::new("opencode");
    cmd.arg("run");
    if let Some(m) = model {
        cmd.args(["--model", m]);
    }
    // Use default format (not json) for interactive output
    cmd.arg(prompt)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to run opencode")
}

/// Run the flow agent and capture the final text output.
pub fn run_flow_agent_capture(prompt: &str) -> Result<String> {
    let gen_loc = find_gen().ok_or_else(|| {
        anyhow::anyhow!(
            "gen not found. Install with:\n  cd {} && f install\n  # or set GEN_REPO env var",
            gen_repo_hint()
        )
    })?;

    if prompt.trim().is_empty() {
        bail!("No prompt provided for flow agent.");
    }

    let full_prompt = build_flow_prompt(prompt)?;
    invoke_gen_capture(&gen_loc, &full_prompt)
}

/// Run the flow agent and stream text output while capturing the final response.
pub fn run_flow_agent_capture_streaming(prompt: &str) -> Result<String> {
    let gen_loc = find_gen().ok_or_else(|| {
        anyhow::anyhow!(
            "gen not found. Install with:\n  cd {} && f install\n  # or set GEN_REPO env var",
            gen_repo_hint()
        )
    })?;

    if prompt.trim().is_empty() {
        bail!("No prompt provided for flow agent.");
    }

    let full_prompt = build_flow_prompt(prompt)?;
    invoke_gen_capture_streaming(&gen_loc, &full_prompt)
}

/// Fallback model if not configured.
const FALLBACK_AGENT_MODEL: &str = "openrouter/moonshotai/kimi-k2:free";

/// Get the agent model from config or use fallback.
fn get_agent_model() -> String {
    if let Some(ts_config) = config::load_ts_config() {
        if let Some(flow) = ts_config.flow {
            if let Some(agents) = flow.agents {
                if let Some(model) = agents.model {
                    return model;
                }
            }
        }
    }
    FALLBACK_AGENT_MODEL.to_string()
}

/// Invoke gen with a prompt and model.
fn invoke_gen(location: &GenLocation, prompt: &str) -> Result<std::process::ExitStatus> {
    let model = get_agent_model();
    invoke_gen_with_model(location, prompt, &model)
}

/// Invoke gen with a prompt and specific model.
fn invoke_gen_with_model(
    location: &GenLocation,
    prompt: &str,
    model: &str,
) -> Result<std::process::ExitStatus> {
    match location {
        GenLocation::Binary(path) => Command::new(path)
            .args(["run", "--model", model, prompt])
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
                "--model",
                model,
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

fn invoke_gen_capture(location: &GenLocation, prompt: &str) -> Result<String> {
    let output = match location {
        GenLocation::Binary(path) => Command::new(path)
            .args(["run", "--format", "json", prompt])
            .stdin(Stdio::null())
            .output()
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
                "--format",
                "json",
                prompt,
            ])
            .env("GEN_MODE", "1")
            .stdin(Stdio::null());
            apply_project_config_env(&mut cmd);
            cmd.output().context("failed to run gen from repo")
        }
    }?;

    if !output.status.success() {
        bail!("gen exited with status: {}", output.status);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(text) = extract_text_from_gen_output(&stdout) {
        return Ok(text);
    }

    let trimmed = stdout.trim();
    if !trimmed.is_empty() {
        return Ok(trimmed.to_string());
    }

    bail!("gen returned no output");
}

fn invoke_gen_capture_streaming(location: &GenLocation, prompt: &str) -> Result<String> {
    let mut cmd = match location {
        GenLocation::Binary(path) => {
            let mut cmd = Command::new(path);
            cmd.args(["run", "--format", "json", prompt])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
            cmd
        }
        GenLocation::Repo(repo) => {
            let mut cmd = Command::new("bun");
            cmd.args([
                "run",
                "--cwd",
                &repo.join("packages/opencode").to_string_lossy(),
                "--conditions=browser",
                "src/index.ts",
                "run",
                "--format",
                "json",
                prompt,
            ])
            .env("GEN_MODE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
            apply_project_config_env(&mut cmd);
            cmd
        }
    };

    let mut child = cmd.spawn().context("failed to run gen")?;
    let stdout = child
        .stdout
        .take()
        .context("failed to capture gen stdout")?;
    let reader = BufReader::new(stdout);

    let mut last_text = String::new();
    let mut final_text = String::new();
    let mut printed_output = false;

    for line in reader.lines() {
        let line = line?;
        if let Some(text) = extract_text_from_gen_line(&line) {
            if text.starts_with(&last_text) {
                let delta = &text[last_text.len()..];
                if !delta.is_empty() {
                    print!("{delta}");
                    io::stdout().flush()?;
                    printed_output = true;
                }
                final_text = text;
            } else {
                if !text.is_empty() {
                    print!("{text}");
                    io::stdout().flush()?;
                    printed_output = true;
                }
                if final_text.is_empty() {
                    final_text = text;
                } else {
                    final_text.push_str(&text);
                }
            }
            last_text = final_text.clone();
        }
    }

    let status = child.wait().context("failed to wait for gen")?;
    if !status.success() {
        bail!("gen exited with status: {}", status);
    }

    if printed_output {
        if !final_text.ends_with('\n') {
            println!();
        }
    }

    if final_text.trim().is_empty() {
        bail!("gen returned no output");
    }

    Ok(final_text)
}

fn extract_text_from_gen_output(stdout: &str) -> Option<String> {
    let mut last_text: Option<String> = None;
    for line in stdout.lines() {
        if let Some(text) = extract_text_from_gen_line(line) {
            if !text.trim().is_empty() {
                last_text = Some(text.to_string());
            }
        }
    }
    last_text
}

fn extract_text_from_gen_line(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    let event_type = value.get("type").and_then(|t| t.as_str());
    if event_type != Some("text") {
        return None;
    }
    value
        .get("part")
        .and_then(|part| part.get("text"))
        .and_then(|t| t.as_str())
        .map(|text| text.to_string())
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
                    context.push_str(&format!(
                        "- `{}`: {} ({})\n",
                        task.task.name, desc, task.task.command
                    ));
                } else {
                    context.push_str(&format!(
                        "- `{}` ({}): {} ({})\n",
                        task.task.name, task.relative_dir, desc, task.task.command
                    ));
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
output_file = "last-build-output.md" # optional: write task output to file

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
7. **output_file**: Capture full task output for debugging or sharing
"#;

/// Flow CLI commands context.
const FLOW_CLI_CONTEXT: &str = r#"

## Flow CLI Commands

- `f` - Fuzzy search tasks (fzf picker)
- `f <task>` - Run task directly
- `f tasks` - List all tasks
- `f run <task> [args]` - Run task with args
- `f init` - Create flow.toml scaffold
- `f setup` - Bootstrap project or run setup task
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
