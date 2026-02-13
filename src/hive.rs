//! Hive agent integration for flow.
//!
//! Agents can be defined at three levels:
//! 1. Project-local: flow.toml [[agents]] or .flow/agents/*.md
//! 2. Global: ~/.config/flow/agents/*.md or ~/.hive/agents/
//! 3. Hive registry: ~/.hive/config.json agents
//!
//! Agent spec format (Markdown):
//! ```markdown
//! # Agent: <name>
//! # Purpose: <description>
//! #
//! # Rules:
//! # - Rule 1
//! # - Rule 2
//! #
//! # Tools:
//! # - bash
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Agent configuration from flow.toml
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AgentConfig {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// System prompt / preamble (inline or file path)
    #[serde(default)]
    pub preamble: Option<String>,
    /// Path to spec file (relative to project root)
    #[serde(default)]
    pub spec: Option<String>,
    /// Tools available to the agent
    #[serde(default)]
    pub tools: Vec<String>,
    /// Model to use (provider-specific)
    #[serde(default)]
    pub model: Option<String>,
    /// Provider: cerebras, deepseek, zai, groq, openrouter
    #[serde(default)]
    pub provider: Option<String>,
    /// Temperature for generation
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Max tokens
    #[serde(default, rename = "max_tokens", alias = "maxTokens")]
    pub max_tokens: Option<u32>,
    /// Max tool call depth
    #[serde(default, rename = "max_depth", alias = "maxDepth")]
    pub max_depth: Option<u32>,
    /// Keywords to match for auto-routing
    #[serde(default, rename = "match_on", alias = "matchOn")]
    pub match_on: Vec<String>,
    /// Context files to include
    #[serde(default)]
    pub context: Vec<String>,
    /// Shortcuts for quick invocation
    #[serde(default)]
    pub shortcuts: Vec<String>,
}

/// Hive global config from ~/.hive/config.json
#[derive(Debug, Clone, Deserialize)]
pub struct HiveConfig {
    #[serde(default)]
    pub agents: HashMap<String, HiveAgentSpec>,
    #[serde(default)]
    pub defaults: Option<HiveDefaults>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HiveAgentSpec {
    #[serde(default)]
    pub job: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default, rename = "matchedOn")]
    pub matched_on: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HiveDefaults {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// Resolved agent from any source
#[derive(Debug, Clone)]
pub struct Agent {
    pub name: String,
    pub source: AgentSource,
    pub spec_path: Option<PathBuf>,
    pub config: AgentConfig,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentSource {
    /// From project flow.toml [[agents]]
    ProjectConfig,
    /// From .flow/agents/<name>.md
    ProjectFile,
    /// From ~/.config/flow/agents/<name>.md
    GlobalFlow,
    /// From ~/.hive/agents/<name>/spec.md
    GlobalHive,
    /// From ~/.hive/config.json
    HiveRegistry,
}

impl std::fmt::Display for AgentSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentSource::ProjectConfig => write!(f, "project"),
            AgentSource::ProjectFile => write!(f, "project"),
            AgentSource::GlobalFlow => write!(f, "global"),
            AgentSource::GlobalHive => write!(f, "hive"),
            AgentSource::HiveRegistry => write!(f, "hive"),
        }
    }
}

/// Load hive global config
pub fn load_hive_config() -> Option<HiveConfig> {
    let path = dirs::home_dir()?.join(".hive/config.json");
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Load project config for agents (best effort, returns default if not found)
fn load_config_for_agents() -> crate::config::Config {
    // Try to find and load flow.toml
    let config_path = PathBuf::from("flow.toml");
    if config_path.exists() {
        if let Ok(cfg) = crate::config::load(&config_path) {
            return cfg;
        }
    }
    // Return default config if not found
    crate::config::Config::default()
}

/// Find agent spec file in standard locations
fn find_agent_spec(name: &str) -> Option<(PathBuf, AgentSource)> {
    // 1. Project-local: .flow/agents/<name>.md
    let project_path = PathBuf::from(".flow/agents").join(format!("{}.md", name));
    if project_path.exists() {
        return Some((project_path, AgentSource::ProjectFile));
    }

    // 2. Global flow: ~/.config/flow/agents/<name>.md
    if let Some(home) = dirs::home_dir() {
        let global_flow = home
            .join(".config/flow/agents")
            .join(format!("{}.md", name));
        if global_flow.exists() {
            return Some((global_flow, AgentSource::GlobalFlow));
        }

        // 3. Hive agents: ~/.hive/agents/<name>/spec.md
        let hive_spec = home.join(".hive/agents").join(name).join("spec.md");
        if hive_spec.exists() {
            return Some((hive_spec, AgentSource::GlobalHive));
        }
    }

    None
}

/// Load agent spec content from file
pub fn load_agent_spec(path: &Path) -> Result<String> {
    fs::read_to_string(path).context(format!("Failed to read agent spec: {}", path.display()))
}

/// Discover all available agents
pub fn discover_agents(project_agents: &[AgentConfig]) -> Vec<Agent> {
    let mut agents = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // 1. Project config agents (highest priority)
    for cfg in project_agents {
        if seen.insert(cfg.name.clone()) {
            let spec_path = cfg.spec.as_ref().map(PathBuf::from);
            agents.push(Agent {
                name: cfg.name.clone(),
                source: AgentSource::ProjectConfig,
                spec_path,
                config: cfg.clone(),
            });
        }
    }

    // 2. Project file agents: .flow/agents/*.md
    if let Ok(entries) = fs::read_dir(".flow/agents") {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().map_or(false, |e| e == "md") {
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string());
                if let Some(name) = stem {
                    if seen.insert(name.clone()) {
                        agents.push(Agent {
                            name: name.clone(),
                            source: AgentSource::ProjectFile,
                            spec_path: Some(path),
                            config: AgentConfig {
                                name,
                                ..Default::default()
                            },
                        });
                    }
                }
            }
        }
    }

    // 3. Global flow agents: ~/.config/flow/agents/*.md
    if let Some(home) = dirs::home_dir() {
        let global_dir = home.join(".config/flow/agents");
        if let Ok(entries) = fs::read_dir(&global_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().map_or(false, |e| e == "md") {
                    let stem = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_string());
                    if let Some(name) = stem {
                        if seen.insert(name.clone()) {
                            agents.push(Agent {
                                name: name.clone(),
                                source: AgentSource::GlobalFlow,
                                spec_path: Some(path),
                                config: AgentConfig {
                                    name,
                                    ..Default::default()
                                },
                            });
                        }
                    }
                }
            }
        }

        // 4. Hive agents: ~/.hive/agents/*/spec.md
        let hive_dir = home.join(".hive/agents");
        if let Ok(entries) = fs::read_dir(&hive_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    let spec_path = path.join("spec.md");
                    if spec_path.exists() {
                        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                            if seen.insert(name.to_string()) {
                                agents.push(Agent {
                                    name: name.to_string(),
                                    source: AgentSource::GlobalHive,
                                    spec_path: Some(spec_path),
                                    config: AgentConfig {
                                        name: name.to_string(),
                                        ..Default::default()
                                    },
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    // 5. Hive registry agents: ~/.hive/config.json
    if let Some(hive_config) = load_hive_config() {
        for (name, spec) in hive_config.agents {
            if seen.insert(name.clone()) {
                agents.push(Agent {
                    name: name.clone(),
                    source: AgentSource::HiveRegistry,
                    spec_path: None,
                    config: AgentConfig {
                        name,
                        description: spec.job.or(spec.prompt),
                        ..Default::default()
                    },
                });
            }
        }
    }

    agents
}

/// Run a hive agent with a prompt
pub fn run_agent(agent: &str, prompt: &str) -> Result<()> {
    // Check if hive is available
    if which::which("hive").is_err() {
        anyhow::bail!("hive not found on PATH. Install from https://github.com/example/hive");
    }

    let status = Command::new("hive")
        .arg("agent")
        .arg(agent)
        .arg(prompt)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to run hive")?;

    if !status.success() {
        anyhow::bail!(
            "hive agent '{}' exited with status {:?}",
            agent,
            status.code()
        );
    }

    Ok(())
}

/// Run an agent interactively (prompt via stdin)
pub fn run_agent_interactive(agent: &str) -> Result<()> {
    if which::which("hive").is_err() {
        anyhow::bail!("hive not found on PATH");
    }

    let status = Command::new("hive")
        .arg("agent")
        .arg(agent)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("Failed to run hive")?;

    if !status.success() {
        anyhow::bail!(
            "hive agent '{}' exited with status {:?}",
            agent,
            status.code()
        );
    }

    Ok(())
}

/// Create a new agent spec file
pub fn create_agent(name: &str, global: bool) -> Result<PathBuf> {
    let path = if global {
        let home = dirs::home_dir().context("Could not find home directory")?;
        let dir = home.join(".hive/agents").join(name);
        fs::create_dir_all(&dir)?;
        dir.join("spec.md")
    } else {
        let dir = PathBuf::from(".flow/agents");
        fs::create_dir_all(&dir)?;
        dir.join(format!("{}.md", name))
    };

    if path.exists() {
        anyhow::bail!("Agent '{}' already exists at {}", name, path.display());
    }

    let template = format!(
        r#"# Agent: {}
# Purpose: <describe what this agent does>
#
# Rules:
# - <rule 1>
# - <rule 2>
#
# Tools:
# - bash
"#,
        name
    );

    fs::write(&path, template)?;
    Ok(path)
}

/// Edit an agent spec file
pub fn edit_agent(name: &str) -> Result<()> {
    let (path, _source) =
        find_agent_spec(name).ok_or_else(|| anyhow::anyhow!("Agent '{}' not found", name))?;

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
    let status = Command::new(&editor)
        .arg(&path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context(format!("Failed to open editor '{}'", editor))?;

    if !status.success() {
        anyhow::bail!("Editor exited with status {:?}", status.code());
    }

    Ok(())
}

/// List agents in a formatted table
pub fn list_agents(project_agents: &[AgentConfig]) {
    let agents = discover_agents(project_agents);

    if agents.is_empty() {
        println!("No agents found.");
        println!("\nCreate one with: f hive new <name>");
        return;
    }

    println!("{:<20} {:<10} {}", "NAME", "SOURCE", "DESCRIPTION");
    println!("{}", "-".repeat(60));

    for agent in agents {
        let desc = agent
            .config
            .description
            .as_deref()
            .unwrap_or("-")
            .chars()
            .take(40)
            .collect::<String>();
        println!("{:<20} {:<10} {}", agent.name, agent.source, desc);
    }
}

/// Get agent by name
pub fn get_agent(name: &str, project_agents: &[AgentConfig]) -> Option<Agent> {
    discover_agents(project_agents)
        .into_iter()
        .find(|a| a.name == name)
}

/// Match agents for auto-routing based on content
pub fn match_agents(content: &str, project_agents: &[AgentConfig], max: usize) -> Vec<String> {
    let content_lower = content.to_lowercase();
    let mut matches = Vec::new();

    // Check project agents first
    for cfg in project_agents {
        if !cfg.match_on.is_empty() {
            let matched = cfg.match_on.iter().any(|term| {
                let needle = term.to_lowercase();
                !needle.is_empty() && content_lower.contains(&needle)
            });
            if matched {
                matches.push(cfg.name.clone());
            }
        }
    }

    // Check hive registry agents
    if let Some(hive_config) = load_hive_config() {
        for (name, spec) in hive_config.agents {
            if let Some(terms) = spec.matched_on {
                let matched = terms.iter().any(|term| {
                    let needle = term.to_lowercase();
                    !needle.is_empty() && content_lower.contains(&needle)
                });
                if matched && !matches.contains(&name) {
                    matches.push(name);
                }
            }
        }
    }

    matches.truncate(max);
    matches
}

/// Handle the `f hive` CLI command.
pub fn run_command(cmd: crate::cli::HiveCommand) -> Result<()> {
    use crate::cli::HiveAction;

    // Load project config to get agents (if available)
    let cfg = load_config_for_agents();

    // Handle direct agent invocation: `f hive fish "wrap ls"`
    if !cmd.agent.is_empty() {
        let agent_name = &cmd.agent[0];
        let prompt = if cmd.agent.len() > 1 {
            cmd.agent[1..].join(" ")
        } else {
            String::new()
        };

        if prompt.is_empty() {
            return run_agent_interactive(agent_name);
        } else {
            return run_agent(agent_name, &prompt);
        }
    }

    match cmd.action {
        None | Some(HiveAction::List) => {
            list_agents(&cfg.agents);
        }
        Some(HiveAction::Run { agent, prompt }) => {
            let prompt_str = prompt.join(" ");
            if prompt_str.is_empty() {
                run_agent_interactive(&agent)?;
            } else {
                run_agent(&agent, &prompt_str)?;
            }
        }
        Some(HiveAction::New { name, global }) => {
            let path = create_agent(&name, global)?;
            println!("Created agent: {}", path.display());
            println!("\nEdit with: f hive edit {}", name);
        }
        Some(HiveAction::Edit { agent }) => {
            if let Some(name) = agent {
                edit_agent(&name)?;
            } else {
                // List agents and ask user to specify
                let agents = discover_agents(&cfg.agents);
                if agents.is_empty() {
                    println!("No agents found. Create one with: f hive new <name>");
                } else {
                    println!("Available agents:");
                    for a in agents {
                        println!("  {}", a.name);
                    }
                    println!("\nRun: f hive edit <agent>");
                }
            }
        }
        Some(HiveAction::Show { agent }) => {
            if let Some(a) = get_agent(&agent, &cfg.agents) {
                if let Some(path) = a.spec_path {
                    let content = load_agent_spec(&path)?;
                    println!("{}", content);
                } else if let Some(desc) = a.config.description {
                    println!("# Agent: {}\n\n{}", agent, desc);
                } else {
                    println!("Agent '{}' has no spec file.", agent);
                }
            } else {
                anyhow::bail!("Agent '{}' not found", agent);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_source_display() {
        assert_eq!(format!("{}", AgentSource::ProjectConfig), "project");
        assert_eq!(format!("{}", AgentSource::GlobalHive), "hive");
    }
}
