//! Match user query to a task using LM Studio.

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

/// Match a user query to a task and optionally execute it.
pub fn run(opts: MatchOpts) -> Result<()> {
    // Check if LM Studio is available
    if !lmstudio::is_available(opts.port) {
        bail!(
            "LM Studio is not running on port {}. Start LM Studio first.",
            opts.port.unwrap_or(1234)
        );
    }

    // Discover all tasks
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let discovery = discover::discover_tasks(&root)?;

    if discovery.tasks.is_empty() {
        bail!("No tasks found in {} or subdirectories", root.display());
    }

    // Build the prompt for LM Studio
    let prompt = build_matching_prompt(&opts.query, &discovery.tasks);

    // Query LM Studio
    let response = lmstudio::quick_prompt(&prompt, opts.model.as_deref(), opts.port)
        .context("failed to get response from LM Studio")?;

    // Parse the response to get the task name
    let task_name = extract_task_name(&response, &discovery.tasks)?;

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
        // Execute the matched task
        let run_opts = TaskRunOpts {
            config: matched.config_path.clone(),
            delegate_to_hub: false,
            hub_host: "127.0.0.1".parse().unwrap(),
            hub_port: 9050,
            name: matched.task.name.clone(),
            args: Vec::new(),
        };
        tasks::run(run_opts)?;
    }

    Ok(())
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
