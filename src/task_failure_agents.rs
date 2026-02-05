use std::collections::HashMap;
use std::fs;
use std::io::IsTerminal;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::Result;
use serde::Deserialize;

use crate::config;

#[derive(Debug, Clone)]
struct TaskFailureSettings {
    enabled: bool,
    tool: String,
    max_lines: usize,
    max_chars: usize,
    max_agents: usize,
}

impl Default for TaskFailureSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            tool: "hive".to_string(),
            max_lines: 80,
            max_chars: 8000,
            max_agents: 2,
        }
    }
}

#[derive(Debug, Deserialize)]
struct HiveConfig {
    agents: Option<HashMap<String, HiveAgentSpec>>,
}

#[derive(Debug, Deserialize)]
struct HiveAgentSpec {
    #[serde(rename = "matchedOn")]
    matched_on: Option<Vec<String>>,
}

fn load_settings() -> TaskFailureSettings {
    let mut settings = TaskFailureSettings::default();
    if let Some(ts_config) = config::load_ts_config() {
        if let Some(flow) = ts_config.flow {
            if let Some(task_failure) = flow.task_failure_agents {
                if let Some(enabled) = task_failure.enabled {
                    settings.enabled = enabled;
                }
                if let Some(tool) = task_failure.tool {
                    if !tool.trim().is_empty() {
                        settings.tool = tool;
                    }
                }
                if let Some(max_lines) = task_failure.max_lines {
                    settings.max_lines = max_lines.max(1);
                }
                if let Some(max_chars) = task_failure.max_chars {
                    settings.max_chars = max_chars.max(100);
                }
                if let Some(max_agents) = task_failure.max_agents {
                    settings.max_agents = max_agents.max(1);
                }
            }
        }
    }
    settings
}

fn load_hive_config() -> Option<HiveConfig> {
    let path = dirs::home_dir()?.join(".hive/config.json");
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn truncate_output(output: &str, max_lines: usize, max_chars: usize) -> String {
    let mut lines: Vec<&str> = output.lines().collect();
    if lines.len() > max_lines {
        lines = lines[lines.len().saturating_sub(max_lines)..].to_vec();
    }
    let mut joined = lines.join("\n");
    if joined.len() > max_chars {
        let start = joined.len().saturating_sub(max_chars);
        joined = format!("...{}", &joined[start..]);
    }
    joined
}

fn matches_agent(haystack: &str, spec: &HiveAgentSpec) -> bool {
    let Some(terms) = &spec.matched_on else {
        return false;
    };
    terms.iter().any(|term| {
        let needle = term.to_lowercase();
        !needle.is_empty() && haystack.contains(&needle)
    })
}

fn run_hive_agent(agent: &str, prompt: &str) -> Result<()> {
    let status = Command::new("hive")
        .arg("agent")
        .arg(agent)
        .arg(prompt)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        eprintln!("⚠ hive agent '{}' exited with status {:?}", agent, status.code());
    }
    Ok(())
}

pub fn maybe_run_task_failure_agents(
    task_name: &str,
    command: &str,
    workdir: &Path,
    output: &str,
    status: Option<i32>,
) {
    if let Ok(value) = std::env::var("FLOW_TASK_FAILURE_AGENTS") {
        let lowered = value.trim().to_lowercase();
        if lowered == "0" || lowered == "false" || lowered == "off" {
            return;
        }
    }
    if std::env::var("FLOW_DISABLE_TASK_FAILURE_AGENTS").is_ok() {
        return;
    }

    let settings = load_settings();
    if !settings.enabled {
        return;
    }
    if settings.tool != "hive" {
        eprintln!("⚠ task-failure agents: unsupported tool '{}'", settings.tool);
        return;
    }
    if !std::io::stdin().is_terminal() {
        return;
    }
    if which::which("hive").is_err() {
        eprintln!("⚠ task-failure agents: hive not found on PATH");
        return;
    }

    let Some(config) = load_hive_config() else {
        eprintln!("⚠ task-failure agents: ~/.hive/config.json not found");
        return;
    };

    let truncated = truncate_output(output, settings.max_lines, settings.max_chars);
    let mut haystack = String::new();
    haystack.push_str(&format!(
        "task: {}\ncommand: {}\nstatus: {}\nworkdir: {}\noutput:\n{}",
        task_name,
        command,
        status.unwrap_or(-1),
        workdir.display(),
        truncated
    ));
    let haystack_lower = haystack.to_lowercase();

    let mut matches: Vec<String> = Vec::new();
    if let Some(agents) = config.agents {
        for (name, spec) in agents {
            if matches_agent(&haystack_lower, &spec) {
                matches.push(name);
            }
        }
    }

    if matches.is_empty() {
        return;
    }

    matches.truncate(settings.max_agents);
    for agent in matches {
        println!("Running agent '{}' for task failure...", agent);
        if let Err(err) = run_hive_agent(&agent, &haystack) {
            eprintln!("⚠ failed to run hive agent '{}': {}", agent, err);
        }
    }
}
