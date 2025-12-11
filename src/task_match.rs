//! Match user query to a task using LM Studio.

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::{
    cli::TaskRunOpts,
    discover::{self, DiscoveredTask},
    lmstudio,
    tasks,
};

/// Options for the match command.
#[derive(Debug, Clone)]
pub struct MatchOpts {
    /// The user's natural language query.
    pub query: String,
    /// LM Studio model to use.
    pub model: Option<String>,
    /// LM Studio API port.
    pub port: Option<u16>,
    /// Whether to actually run the matched task.
    pub execute: bool,
}

/// Result of matching a query to a task.
#[derive(Debug)]
pub struct MatchResult {
    pub task_name: String,
    pub config_path: PathBuf,
    pub relative_dir: String,
}

// Built-in commands that can be run directly if no task matches
const BUILTIN_COMMANDS: &[(&str, &[&str])] = &[
    ("commit", &["commit", "c"]),
];

// CLI subcommands that should be passed through to the CLI parser, not matched as tasks
const CLI_SUBCOMMANDS: &[&str] = &[
    "logs", "hub", "ps", "kill", "projects", "active", "server", "init", "doctor",
    "tasks", "search", "rerun", "last-cmd", "last-cmd-full", "match", "help",
];

fn run_builtin(name: &str, execute: bool) -> Result<()> {
    match name {
        "commit" => {
            println!("Running: commit");
            if execute {
                crate::commit::run(true)?;
            }
        }
        _ => bail!("Unknown built-in: {}", name),
    }
    Ok(())
}

fn find_builtin(query: &str) -> Option<&'static str> {
    let q = query.trim().to_lowercase();
    for (name, aliases) in BUILTIN_COMMANDS {
        if aliases.iter().any(|a| *a == q) {
            return Some(name);
        }
    }
    None
}

/// Check if query starts with a CLI subcommand that needs pass-through
fn is_cli_subcommand(query: &str) -> bool {
    let first_word = query.split_whitespace().next().unwrap_or("");
    CLI_SUBCOMMANDS.iter().any(|cmd| cmd.eq_ignore_ascii_case(first_word))
}

/// Re-invoke the CLI with the original arguments (bypassing match)
fn passthrough_to_cli(query: &str) -> Result<()> {
    use std::process::Command;

    let exe = std::env::current_exe().context("failed to get current executable")?;
    let args: Vec<&str> = query.split_whitespace().collect();

    let status = Command::new(&exe)
        .args(&args)
        .status()
        .with_context(|| format!("failed to run: {} {}", exe.display(), args.join(" ")))?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Match a user query to a task and optionally execute it.
pub fn run(opts: MatchOpts) -> Result<()> {
    // Check if this is a CLI subcommand that should bypass matching
    if is_cli_subcommand(&opts.query) {
        return passthrough_to_cli(&opts.query);
    }

    // Discover all tasks
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let discovery = discover::discover_tasks(&root)?;

    // Try direct match first (exact name, shortcut, or abbreviation) - no LLM needed
    let (task_name, task_args, was_direct_match) =
        if let Some(direct) = try_direct_match(&opts.query, &discovery.tasks) {
            (direct.task_name, direct.args, true)
        } else if let Some(builtin) = find_builtin(&opts.query) {
            // No task match, but matches a built-in command
            return run_builtin(builtin, opts.execute);
        } else if discovery.tasks.is_empty() {
            // No tasks and no built-in match
            bail!("No tasks found in {} or subdirectories", root.display());
        } else {
            // No direct match, use LM Studio
            let prompt = build_matching_prompt(&opts.query, &discovery.tasks);

            // Query LM Studio (will fail with clear error if not running)
            let response = match lmstudio::quick_prompt(&prompt, opts.model.as_deref(), opts.port) {
                Ok(r) if !r.trim().is_empty() => r,
                Ok(_) => {
                    // Empty response - check for built-in before failing
                    if let Some(builtin) = find_builtin(&opts.query) {
                        return run_builtin(builtin, opts.execute);
                    }
                    let task_list: Vec<_> = discovery.tasks.iter().map(|t| t.task.name.as_str()).collect();
                    bail!(
                        "No match for '{}'. LM Studio returned empty response.\n\nAvailable tasks:\n  {}",
                        opts.query,
                        task_list.join("\n  ")
                    );
                }
                Err(e) => {
                    // LM Studio error - fall back to built-in if available
                    if let Some(builtin) = find_builtin(&opts.query) {
                        return run_builtin(builtin, opts.execute);
                    }
                    let task_list: Vec<_> = discovery.tasks.iter().map(|t| t.task.name.as_str()).collect();
                    bail!(
                        "No direct match for '{}'. LM Studio error: {}\n\nAvailable tasks:\n  {}",
                        opts.query,
                        e,
                        task_list.join("\n  ")
                    );
                }
            };

            // Parse the response to get the task name (no args for LLM matches)
            (extract_task_name(&response, &discovery.tasks)?, Vec::new(), false)
        };

    // Find the matched task
    let matched = discovery
        .tasks
        .iter()
        .find(|t| t.task.name.eq_ignore_ascii_case(&task_name))
        .ok_or_else(|| anyhow::anyhow!("LM Studio returned unknown task: {}", task_name))?;

    // Show what was matched
    if matched.relative_dir.is_empty() {
        println!("Matched: {} – {}", matched.task.name, matched.task.command);
    } else {
        println!(
            "Matched: {} ({}) – {}",
            matched.task.name, matched.relative_dir, matched.task.command
        );
    }

    if opts.execute {
        // Check if confirmation is needed (only for LLM matches on tasks with confirm_on_match)
        let needs_confirm = !was_direct_match && matched.task.confirm_on_match;

        if needs_confirm {
            print!("Press Enter to confirm, Ctrl+C to cancel: ");
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
        }

        // Execute the matched task
        let run_opts = TaskRunOpts {
            config: matched.config_path.clone(),
            delegate_to_hub: false,
            hub_host: "127.0.0.1".parse().unwrap(),
            hub_port: 9050,
            name: matched.task.name.clone(),
            args: task_args.clone(),
        };
        tasks::run(run_opts)?;
    }

    Ok(())
}

/// Normalize a string by removing hyphens, underscores, and lowercasing
fn normalize_name(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '-' && *c != '_')
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Result of a direct match attempt - includes task name and any extra args
struct DirectMatchResult {
    task_name: String,
    args: Vec<String>,
}

/// Try to match query directly to a task name, shortcut, or abbreviation.
/// Returns the task name and any remaining arguments.
fn try_direct_match(query: &str, tasks: &[DiscoveredTask]) -> Option<DirectMatchResult> {
    let query = query.trim();
    let parts: Vec<&str> = query.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }

    let first = parts[0];
    let rest: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

    // Exact name match (case-insensitive)
    if let Some(task) = tasks
        .iter()
        .find(|t| t.task.name.eq_ignore_ascii_case(first))
    {
        return Some(DirectMatchResult {
            task_name: task.task.name.clone(),
            args: rest,
        });
    }

    // Shortcut match
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

    // Normalized match (ignoring hyphens/underscores, only if unambiguous)
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

    // Abbreviation match (only if unambiguous)
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

fn build_matching_prompt(query: &str, tasks: &[DiscoveredTask]) -> String {
    let mut prompt = String::new();

    prompt.push_str(
        "You are a task matcher. Given a user query, select the most appropriate task from the list below.\n\n",
    );

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

    prompt.push_str("\nRespond with ONLY the exact task name, nothing else. No explanation.\n");
    prompt.push_str(&format!("\nUser query: {}\n", query));
    prompt.push_str("\nTask name:");

    prompt
}

fn extract_task_name(response: &str, tasks: &[DiscoveredTask]) -> Result<String> {
    let response = response.trim();

    // Try exact match first
    for task in tasks {
        if task.task.name.eq_ignore_ascii_case(response) {
            return Ok(task.task.name.clone());
        }
    }

    // Try to find a task name within the response
    for task in tasks {
        if response.to_lowercase().contains(&task.task.name.to_lowercase()) {
            return Ok(task.task.name.clone());
        }
    }

    // Clean up common LLM artifacts
    let cleaned = response
        .trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .to_string();

    for task in tasks {
        if task.task.name.eq_ignore_ascii_case(&cleaned) {
            return Ok(task.task.name.clone());
        }
    }

    bail!(
        "Could not parse task name from LM response: '{}'\nAvailable tasks: {}",
        response,
        tasks
            .iter()
            .map(|t| t.task.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TaskConfig;

    fn make_discovered(name: &str, desc: Option<&str>) -> DiscoveredTask {
        DiscoveredTask {
            task: TaskConfig {
                name: name.to_string(),
                command: format!("echo {}", name),
                delegate_to_hub: false,
                activate_on_cd_to_root: false,
                dependencies: Vec::new(),
                description: desc.map(|s| s.to_string()),
                shortcuts: Vec::new(),
                interactive: false,
                confirm_on_match: false,
            },
            config_path: PathBuf::from("flow.toml"),
            relative_dir: String::new(),
            depth: 0,
        }
    }

    #[test]
    fn extracts_exact_task_name() {
        let tasks = vec![
            make_discovered("build", Some("Build the project")),
            make_discovered("test", Some("Run tests")),
        ];

        assert_eq!(extract_task_name("build", &tasks).unwrap(), "build");
        assert_eq!(extract_task_name("BUILD", &tasks).unwrap(), "build");
        assert_eq!(extract_task_name("  test  ", &tasks).unwrap(), "test");
    }

    #[test]
    fn extracts_task_name_from_response() {
        let tasks = vec![
            make_discovered("build", None),
            make_discovered("deploy-prod", None),
        ];

        assert_eq!(
            extract_task_name("The task is: build", &tasks).unwrap(),
            "build"
        );
        assert_eq!(
            extract_task_name("deploy-prod.", &tasks).unwrap(),
            "deploy-prod"
        );
    }
}
