//! Ask the AI server to suggest a task or flow command.

use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use clap::CommandFactory;

use crate::ai_server;
use crate::cli::Cli;
use crate::discover::{self, DiscoveredTask};
use crate::{config, external_cli, opentui_prompt};

const RUN_AGENT_ROUTER_PATH: &str = "~/run/scripts/agent-router.sh";

/// Options for the ask command.
#[derive(Debug, Clone)]
pub struct AskOpts {
    /// The user's query as separate arguments (preserves quoting from shell).
    pub args: Vec<String>,
    /// AI server model to use.
    pub model: Option<String>,
    /// AI server URL override.
    pub url: Option<String>,
}

enum AskSelection {
    Task { name: String },
    Command { command: String },
}

struct FlowCommand {
    name: String,
    aliases: Vec<String>,
    about: Option<String>,
}

struct AskRoutingContext {
    cwd: PathBuf,
    external_clis: Vec<AskExternalCli>,
    codex_agents: Vec<String>,
}

struct AskExternalCli {
    id: String,
    description: Option<String>,
}

/// Ask the AI server for a suggested task or command.
pub fn run(opts: AskOpts) -> Result<()> {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let discovery = discover::discover_tasks(&root)?;
    run_with_tasks(opts, discovery.tasks)
}

fn run_with_tasks(opts: AskOpts, tasks: Vec<DiscoveredTask>) -> Result<()> {
    let query_display = opts.args.join(" ");
    let routing = load_routing_context();

    if let Some(direct) = try_direct_match(&opts.args, &tasks) {
        let matched = find_task(&direct.task_name, &tasks)?;
        suggest_task(matched, &direct.args)?;
        return Ok(());
    }

    if is_cli_subcommand(&opts.args) {
        let command = format!("f {}", opts.args.join(" "));
        suggest_command(&command, &[])?;
        return Ok(());
    }

    if let Some(command) = route_special_query(&query_display, &routing) {
        let details = vec!["Matched built-in Codex session routing.".to_string()];
        suggest_command(&command, &details)?;
        return Ok(());
    }

    let commands = flow_command_candidates();
    let valid_subcommands = valid_subcommand_set(&commands);
    let prompt = build_prompt(&query_display, &tasks, &commands, &routing);

    let response =
        ai_server::quick_prompt(&prompt, opts.model.as_deref(), opts.url.as_deref(), None)?;

    let selection = parse_ask_response(&response, &tasks, &valid_subcommands)?;

    match selection {
        AskSelection::Task { name } => {
            let matched = find_task(&name, &tasks)?;
            suggest_task(matched, &[])?;
        }
        AskSelection::Command { command } => {
            suggest_command(&command, &[])?;
        }
    }

    Ok(())
}

fn suggest_task(task: &DiscoveredTask, args: &[String]) -> Result<()> {
    let command = if args.is_empty() {
        format!("f {}", task.task.name)
    } else {
        format!("f {} {}", task.task.name, args.join(" "))
    };

    let mut detail = format!("Matched task: {}", task.task.name);
    if !task.relative_dir.is_empty() {
        detail.push_str(&format!(" ({})", task.relative_dir));
    }
    detail.push_str(&format!(" - {}", task.task.command));
    suggest_command(&command, &[detail])
}

fn suggest_command(command: &str, details: &[String]) -> Result<()> {
    println!("Suggested command:");
    println!("{}", command.trim());
    for detail in details {
        println!("{}", detail);
    }
    maybe_offer_execute(command)
}

fn find_task<'a>(name: &str, tasks: &'a [DiscoveredTask]) -> Result<&'a DiscoveredTask> {
    tasks
        .iter()
        .find(|t| t.task.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| anyhow::anyhow!("AI returned unknown task: {}", name))
}

fn load_routing_context() -> AskRoutingContext {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let external_clis = external_cli::list_external_cli_tools()
        .unwrap_or_default()
        .into_iter()
        .map(|tool| AskExternalCli {
            id: tool.manifest.id,
            description: tool.manifest.description,
        })
        .collect();
    let codex_agents = load_codex_agent_ids().unwrap_or_default();

    AskRoutingContext {
        cwd,
        external_clis,
        codex_agents,
    }
}

fn load_codex_agent_ids() -> Result<Vec<String>> {
    let router_path = config::expand_path(RUN_AGENT_ROUTER_PATH);
    if !router_path.is_file() {
        return Ok(Vec::new());
    }

    let output = Command::new("bash")
        .arg(&router_path)
        .arg("list")
        .output()?;
    if !output.status.success() {
        return Ok(Vec::new());
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn route_special_query(query: &str, routing: &AskRoutingContext) -> Option<String> {
    let query_lower = query.to_ascii_lowercase();
    let target_path = extract_path_hint(query).unwrap_or_else(|| routing.cwd.clone());
    let target = target_path.display().to_string();

    let session_like = query_lower.contains("session")
        && (query_lower.contains("my ")
            || query_lower.contains(" current")
            || query_lower.contains("what is")
            || query_lower.contains("which session")
            || query_lower.contains("browse"));
    if session_like {
        return Some(format!(
            "f ai codex browse --path {}",
            shell_words::quote(&target)
        ));
    }

    None
}

fn extract_path_hint(query: &str) -> Option<PathBuf> {
    for raw in query.split_whitespace() {
        let token = raw.trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\''
                    | '`'
                    | ','
                    | '.'
                    | ';'
                    | ':'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '?'
                    | '!'
            )
        });
        if !(token.starts_with("~/") || token.starts_with('/')) {
            continue;
        }
        let expanded = config::expand_path(token);
        if expanded.exists() {
            return Some(expanded);
        }
    }
    None
}

fn maybe_offer_execute(command: &str) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(());
    }

    let lines = vec![
        "Run this command now?".to_string(),
        command.trim().to_string(),
    ];
    if !confirm_with_tui("Ask", &lines, "Run suggested command? [Y/n]: ")? {
        return Ok(());
    }

    execute_suggested_command(command)
}

fn execute_suggested_command(command: &str) -> Result<()> {
    let tokens = shell_words::split(command).unwrap_or_else(|_| {
        command
            .split_whitespace()
            .map(|part| part.to_string())
            .collect()
    });
    if tokens.is_empty() {
        bail!("Suggested command is empty.");
    }

    let args = match tokens.first().map(|token| token.as_str()) {
        Some("f") | Some("flow") => tokens[1..].to_vec(),
        _ => tokens,
    };
    if args.is_empty() {
        bail!("Suggested command is incomplete.");
    }

    let exe = std::env::current_exe()?;
    let status = Command::new(&exe)
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to execute suggested command via {}", exe.display()))?;
    if !status.success() {
        bail!(
            "suggested command exited unsuccessfully with status {}",
            status
        );
    }
    Ok(())
}

fn confirm_with_tui(title: &str, lines: &[String], prompt: &str) -> Result<bool> {
    if let Some(answer) = opentui_prompt::confirm(title, lines, true) {
        return Ok(answer);
    }
    confirm_default_yes(prompt)
}

fn confirm_default_yes(prompt: &str) -> Result<bool> {
    print!("{}", prompt);
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();
    if answer.is_empty() {
        return Ok(true);
    }
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

fn flow_command_candidates() -> Vec<FlowCommand> {
    let mut commands = Vec::new();
    let cmd = Cli::command();
    for sub in cmd.get_subcommands() {
        let name = sub.get_name().to_string();
        let about = sub
            .get_about()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        let aliases = sub.get_all_aliases().map(|a| a.to_string()).collect();
        commands.push(FlowCommand {
            name,
            aliases,
            about,
        });
    }

    commands.push(FlowCommand {
        name: "tasks list".to_string(),
        aliases: Vec::new(),
        about: Some("List tasks from flow.toml.".to_string()),
    });

    commands.sort_by(|a, b| a.name.cmp(&b.name));
    commands
}

fn valid_subcommand_set(commands: &[FlowCommand]) -> HashSet<String> {
    let mut set = HashSet::new();
    for cmd in commands {
        let name = cmd.name.split_whitespace().next().unwrap_or("").to_string();
        if !name.is_empty() {
            set.insert(name.to_ascii_lowercase());
        }
        for alias in &cmd.aliases {
            set.insert(alias.to_ascii_lowercase());
        }
    }
    set.insert("help".to_string());
    set.insert("-h".to_string());
    set.insert("--help".to_string());
    set
}

fn build_prompt(
    query: &str,
    tasks: &[DiscoveredTask],
    commands: &[FlowCommand],
    routing: &AskRoutingContext,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("You are a Flow CLI assistant.\n");
    prompt.push_str("Choose the best command for the user to run.\n");
    prompt.push_str("Respond with ONE line in one of these formats only:\n");
    prompt.push_str("task:<task_name>\n");
    prompt.push_str("cmd:f <flow command>\n\n");
    prompt.push_str(&format!("Current workspace: {}\n\n", routing.cwd.display()));

    if tasks.is_empty() {
        prompt.push_str("No flow.toml tasks were discovered.\n");
    } else {
        prompt.push_str("Available tasks:\n");
        for task in tasks {
            let location = if task.relative_dir.is_empty() {
                String::new()
            } else {
                format!(" (in {})", task.relative_dir)
            };
            let desc = task
                .task
                .description
                .as_deref()
                .unwrap_or(&task.task.command);
            prompt.push_str(&format!("- {}{}: {}\n", task.task.name, location, desc));
        }
    }

    prompt.push_str("\nFlow CLI commands:\n");
    for cmd in commands {
        let mut line = format!("- f {}", cmd.name);
        if let Some(about) = &cmd.about {
            if !about.trim().is_empty() {
                line.push_str(&format!(": {}", about.trim()));
            }
        }
        prompt.push_str(&format!("{}\n", line));
    }

    prompt.push_str("\nSpecial command templates:\n");
    prompt.push_str(&format!(
        "- f ai codex browse --path {}: browse and select Codex sessions for the current workspace\n",
        shell_words::quote(&routing.cwd.display().to_string())
    ));

    for tool in &routing.external_clis {
        let mut line = format!("- f cli {} -- ...: run external CLI {}", tool.id, tool.id);
        if let Some(description) = &tool.description {
            if !description.trim().is_empty() {
                line.push_str(&format!(" ({})", description.trim()));
            }
        }
        prompt.push_str(&format!("{line}\n"));
    }

    if !routing.codex_agents.is_empty() {
        prompt.push_str("\nRun-owned Codex agents:\n");
        for agent_id in &routing.codex_agents {
            prompt.push_str(&format!(
                "- f ai codex agent run --path {} {} \"<query>\": run the {} agent for the current workspace\n",
                shell_words::quote(&routing.cwd.display().to_string()),
                agent_id,
                agent_id
            ));
        }
    }

    prompt.push_str(&format!("\nUser query: {}\n", query));
    prompt.push_str("Answer:");

    prompt
}

fn parse_ask_response(
    response: &str,
    tasks: &[DiscoveredTask],
    valid_subcommands: &HashSet<String>,
) -> Result<AskSelection> {
    let cleaned = response.trim().trim_matches('`').trim();
    if cleaned.is_empty() {
        bail!("AI returned an empty response.");
    }

    if let Some(selection) = parse_structured_line(cleaned, tasks, valid_subcommands)? {
        return Ok(selection);
    }

    // Some models emit reasoning wrappers (e.g. <think>...</think>) before the
    // final machine-readable answer. Scan lines and parse the first valid one.
    for line in cleaned.lines() {
        let candidate = line.trim();
        if candidate.is_empty() {
            continue;
        }
        if let Some(selection) = parse_structured_line(candidate, tasks, valid_subcommands)? {
            return Ok(selection);
        }
    }

    if cleaned.starts_with("f ") || cleaned.starts_with("flow ") {
        let command = normalize_command(cleaned, valid_subcommands)?;
        return Ok(AskSelection::Command { command });
    }

    if let Ok(task_name) = extract_task_name(cleaned, tasks) {
        return Ok(AskSelection::Task { name: task_name });
    }

    if is_command_like(cleaned, valid_subcommands) {
        let command = normalize_command(cleaned, valid_subcommands)?;
        return Ok(AskSelection::Command { command });
    }

    bail!("Could not parse AI response: '{}'", cleaned);
}

fn parse_structured_line(
    raw: &str,
    tasks: &[DiscoveredTask],
    valid_subcommands: &HashSet<String>,
) -> Result<Option<AskSelection>> {
    if let Some(rest) = raw.strip_prefix("task:") {
        let task_name = extract_task_name(rest.trim(), tasks)?;
        return Ok(Some(AskSelection::Task { name: task_name }));
    }
    if let Some(rest) = raw.strip_prefix("cmd:") {
        let command = normalize_command(rest, valid_subcommands)?;
        return Ok(Some(AskSelection::Command { command }));
    }
    if let Some(rest) = raw.strip_prefix("command:") {
        let command = normalize_command(rest, valid_subcommands)?;
        return Ok(Some(AskSelection::Command { command }));
    }
    Ok(None)
}

fn normalize_command(raw: &str, valid_subcommands: &HashSet<String>) -> Result<String> {
    let mut cmd = raw.trim().trim_matches('`').trim().to_string();
    if cmd.starts_with("cmd:") {
        cmd = cmd.trim_start_matches("cmd:").trim().to_string();
    } else if cmd.starts_with("command:") {
        cmd = cmd.trim_start_matches("command:").trim().to_string();
    }

    if cmd.starts_with("flow ") {
        cmd = format!("f {}", cmd.trim_start_matches("flow ").trim());
    } else if !cmd.starts_with("f ") {
        cmd = format!("f {}", cmd);
    }

    let tokens = shell_words::split(&cmd)
        .unwrap_or_else(|_| cmd.split_whitespace().map(|s| s.to_string()).collect());
    if tokens.len() < 2 {
        bail!("Command '{}' is incomplete.", cmd);
    }
    let sub = tokens[1].to_ascii_lowercase();
    if !valid_subcommands.contains(&sub) {
        bail!("AI returned unknown command '{}'.", cmd);
    }

    Ok(cmd)
}

fn is_command_like(raw: &str, valid_subcommands: &HashSet<String>) -> bool {
    let first = raw
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if first.is_empty() {
        return false;
    }
    valid_subcommands.contains(&first)
}

fn cli_subcommands() -> Vec<String> {
    let mut names = Vec::new();
    let cmd = Cli::command();
    for sub in cmd.get_subcommands() {
        names.push(sub.get_name().to_string());
        for alias in sub.get_all_aliases() {
            names.push(alias.to_string());
        }
    }
    names
}

fn is_cli_subcommand(args: &[String]) -> bool {
    let Some(first) = args.first() else {
        return false;
    };
    let first_lower = first.to_ascii_lowercase();
    cli_subcommands()
        .iter()
        .any(|cmd| cmd.eq_ignore_ascii_case(&first_lower))
}

/// Normalize a string by removing hyphens, underscores, and lowercasing.
fn normalize_name(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '-' && *c != '_')
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Result of a direct match attempt - includes task name and any extra args.
struct DirectMatchResult {
    task_name: String,
    args: Vec<String>,
}

/// Try to match query directly to a task name, shortcut, or abbreviation.
fn try_direct_match(args: &[String], tasks: &[DiscoveredTask]) -> Option<DirectMatchResult> {
    if args.is_empty() {
        return None;
    }

    let first = args[0].trim();
    let rest: Vec<String> = args[1..].to_vec();

    if let Some(task) = tasks
        .iter()
        .find(|t| t.task.name.eq_ignore_ascii_case(first))
    {
        return Some(DirectMatchResult {
            task_name: task.task.name.clone(),
            args: rest,
        });
    }

    if let Some(task) = tasks.iter().find(|t| {
        t.task
            .shortcuts
            .iter()
            .any(|s| s.eq_ignore_ascii_case(first))
    }) {
        return Some(DirectMatchResult {
            task_name: task.task.name.clone(),
            args: rest,
        });
    }

    let normalized_query = normalize_name(first);
    let mut normalized_matches: Vec<_> = tasks
        .iter()
        .filter(|t| normalize_name(&t.task.name) == normalized_query)
        .collect();
    if normalized_matches.len() == 1 {
        return Some(DirectMatchResult {
            task_name: normalized_matches.remove(0).task.name.clone(),
            args: rest,
        });
    }

    let needle = first.to_ascii_lowercase();
    if needle.len() >= 2 {
        let mut matches = tasks.iter().filter(|t| {
            generate_abbreviation(&t.task.name)
                .map(|abbr| abbr == needle)
                .unwrap_or(false)
        });

        if let Some(first_match) = matches.next() {
            if matches.next().is_none() {
                return Some(DirectMatchResult {
                    task_name: first_match.task.name.clone(),
                    args: rest,
                });
            }
        }
    }

    if needle.len() >= 2 {
        let mut prefix_matches: Vec<_> = tasks
            .iter()
            .filter(|t| t.task.name.to_ascii_lowercase().starts_with(&needle))
            .collect();
        if prefix_matches.len() == 1 {
            return Some(DirectMatchResult {
                task_name: prefix_matches.remove(0).task.name.clone(),
                args: rest,
            });
        }
    }

    None
}

fn generate_abbreviation(name: &str) -> Option<String> {
    let mut abbr = String::new();
    let mut new_segment = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if new_segment {
                abbr.push(ch.to_ascii_lowercase());
                new_segment = false;
            }
        } else {
            new_segment = true;
        }
    }
    if abbr.len() >= 2 { Some(abbr) } else { None }
}

fn extract_task_name(response: &str, tasks: &[DiscoveredTask]) -> Result<String> {
    let response = response.trim();

    for task in tasks {
        if task.task.name.eq_ignore_ascii_case(response) {
            return Ok(task.task.name.clone());
        }
    }

    for task in tasks {
        if response
            .to_lowercase()
            .contains(&task.task.name.to_lowercase())
        {
            return Ok(task.task.name.clone());
        }
    }

    let cleaned = response
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .to_string();

    for task in tasks {
        if task.task.name.eq_ignore_ascii_case(&cleaned) {
            return Ok(task.task.name.clone());
        }
    }

    bail!("Could not parse task name from AI response: '{}'", response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TaskConfig;

    fn make_discovered(name: &str) -> DiscoveredTask {
        DiscoveredTask {
            task: TaskConfig {
                name: name.to_string(),
                command: format!("echo {}", name),
                delegate_to_hub: false,
                activate_on_cd_to_root: false,
                dependencies: Vec::new(),
                description: None,
                shortcuts: Vec::new(),
                interactive: false,
                confirm_on_match: false,
                on_cancel: None,
                output_file: None,
            },
            config_path: PathBuf::from("flow.toml"),
            relative_dir: String::new(),
            depth: 0,
            scope: "root".to_string(),
            scope_aliases: vec!["root".to_string()],
        }
    }

    #[test]
    fn parse_task_response() {
        let tasks = vec![make_discovered("build")];
        let mut cmds = HashSet::new();
        cmds.insert("tasks".to_string());
        let parsed = parse_ask_response("task:build", &tasks, &cmds).unwrap();
        match parsed {
            AskSelection::Task { name } => assert_eq!(name, "build"),
            _ => panic!("expected task"),
        }
    }

    #[test]
    fn parse_command_response() {
        let tasks = vec![make_discovered("build")];
        let mut cmds = HashSet::new();
        cmds.insert("tasks".to_string());
        let parsed = parse_ask_response("cmd:f tasks list", &tasks, &cmds).unwrap();
        match parsed {
            AskSelection::Command { command } => assert_eq!(command, "f tasks list"),
            _ => panic!("expected command"),
        }
    }

    #[test]
    fn special_session_query_routes_to_codex_browser() {
        let routing = AskRoutingContext {
            cwd: PathBuf::from("/tmp/example"),
            external_clis: Vec::new(),
            codex_agents: Vec::new(),
        };

        let command = route_special_query("what is my session", &routing).expect("route");
        assert_eq!(command, "f ai codex browse --path /tmp/example");
    }

    #[test]
    fn special_session_query_prefers_explicit_path_hint() {
        let temp = tempfile::tempdir().expect("tempdir");
        let routing = AskRoutingContext {
            cwd: PathBuf::from("/tmp/example"),
            external_clis: Vec::new(),
            codex_agents: Vec::new(),
        };

        let query = format!("what is my session for {}", temp.path().display());
        let command = route_special_query(&query, &routing).expect("route");
        assert_eq!(
            command,
            format!("f ai codex browse --path {}", temp.path().display())
        );
    }
}
