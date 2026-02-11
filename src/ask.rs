//! Ask the AI server to suggest a task or flow command.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::CommandFactory;

use crate::ai_server;
use crate::cli::Cli;
use crate::discover::{self, DiscoveredTask};

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

/// Ask the AI server for a suggested task or command.
pub fn run(opts: AskOpts) -> Result<()> {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let discovery = discover::discover_tasks(&root)?;
    run_with_tasks(opts, discovery.tasks)
}

fn run_with_tasks(opts: AskOpts, tasks: Vec<DiscoveredTask>) -> Result<()> {
    let query_display = opts.args.join(" ");

    if let Some(direct) = try_direct_match(&opts.args, &tasks) {
        let matched = find_task(&direct.task_name, &tasks)?;
        print_task_suggestion(matched, &direct.args);
        return Ok(());
    }

    if is_cli_subcommand(&opts.args) {
        let command = format!("f {}", opts.args.join(" "));
        print_command_suggestion(&command);
        return Ok(());
    }

    let commands = flow_command_candidates();
    let valid_subcommands = valid_subcommand_set(&commands);
    let prompt = build_prompt(&query_display, &tasks, &commands);

    let response =
        ai_server::quick_prompt(&prompt, opts.model.as_deref(), opts.url.as_deref(), None)?;

    let selection = parse_ask_response(&response, &tasks, &valid_subcommands)?;

    match selection {
        AskSelection::Task { name } => {
            let matched = find_task(&name, &tasks)?;
            print_task_suggestion(matched, &[]);
        }
        AskSelection::Command { command } => {
            print_command_suggestion(&command);
        }
    }

    Ok(())
}

fn print_task_suggestion(task: &DiscoveredTask, args: &[String]) {
    let command = if args.is_empty() {
        format!("f {}", task.task.name)
    } else {
        format!("f {} {}", task.task.name, args.join(" "))
    };

    println!("Suggested command:");
    println!("{}", command);

    let mut detail = format!("Matched task: {}", task.task.name);
    if !task.relative_dir.is_empty() {
        detail.push_str(&format!(" ({})", task.relative_dir));
    }
    detail.push_str(&format!(" - {}", task.task.command));
    println!("{}", detail);
}

fn print_command_suggestion(command: &str) {
    println!("Suggested command:");
    println!("{}", command.trim());
}

fn find_task<'a>(name: &str, tasks: &'a [DiscoveredTask]) -> Result<&'a DiscoveredTask> {
    tasks
        .iter()
        .find(|t| t.task.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| anyhow::anyhow!("AI returned unknown task: {}", name))
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

fn build_prompt(query: &str, tasks: &[DiscoveredTask], commands: &[FlowCommand]) -> String {
    let mut prompt = String::new();
    prompt.push_str("You are a Flow CLI assistant.\n");
    prompt.push_str("Choose the best command for the user to run.\n");
    prompt.push_str("Respond with ONE line in one of these formats only:\n");
    prompt.push_str("task:<task_name>\n");
    prompt.push_str("cmd:f <flow command>\n\n");

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

    if let Some(rest) = cleaned.strip_prefix("task:") {
        let task_name = extract_task_name(rest.trim(), tasks)?;
        return Ok(AskSelection::Task { name: task_name });
    }

    if let Some(rest) = cleaned.strip_prefix("cmd:") {
        let command = normalize_command(rest, valid_subcommands)?;
        return Ok(AskSelection::Command { command });
    }

    if let Some(rest) = cleaned.strip_prefix("command:") {
        let command = normalize_command(rest, valid_subcommands)?;
        return Ok(AskSelection::Command { command });
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
}
