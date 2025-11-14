use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Top-level configuration for flowd, currently focused on managed servers.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
    #[serde(default)]
    pub tasks: Vec<TaskConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            tasks: Vec::new(),
        }
    }
}

/// Configuration for a single managed HTTP server process.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Human-friendly name used in the TUI and HTTP API.
    pub name: String,
    /// Program to execute, e.g. "node", "cargo".
    pub command: String,
    /// Arguments passed to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional working directory for the process.
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    /// Additional environment variables.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Whether this server should be started automatically with the daemon.
    #[serde(default = "default_autostart")]
    pub autostart: bool,
}

fn default_autostart() -> bool {
    true
}

/// Local project automation task description.
#[derive(Debug, Clone, Deserialize)]
pub struct TaskConfig {
    /// Unique identifier for the task (used when selecting it interactively).
    pub name: String,
    /// Shell command that should be executed for this task.
    pub command: String,
    /// Optional human-friendly description.
    #[serde(default, alias = "desc")]
    pub description: Option<String>,
}

/// Default config path: ~/.config/flow/config.toml
pub fn default_config_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let mut path = PathBuf::from(home);
        path.push(".config/flow/config.toml");
        path
    } else {
        PathBuf::from(".config/flow/config.toml")
    }
}

pub fn load<P: AsRef<Path>>(path: P) -> Result<Config> {
    let path = path.as_ref();
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read flow config at {}", path.display()))?;
    let cfg: Config = toml::from_str(&contents)
        .with_context(|| format!("failed to parse flow config at {}", path.display()))?;
    Ok(cfg)
}

/// Load config from the given path, logging a warning and returning an empty
/// config if anything goes wrong. This keeps the daemon usable even if the
/// config file is missing or invalid.
pub fn load_or_default<P: AsRef<Path>>(path: P) -> Config {
    match load(path) {
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::warn!(
                ?err,
                "failed to load flow config; starting with no managed servers"
            );
            Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn fixture_path(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
    }

    #[test]
    fn load_parses_global_fixture() {
        let cfg = load(fixture_path("test-data/global-config/flow.toml"))
            .expect("global config fixture should parse");

        assert_eq!(cfg.servers.len(), 2);
        assert!(
            cfg.tasks.is_empty(),
            "global config should not define tasks"
        );

        let frontend = &cfg.servers[0];
        assert_eq!(frontend.name, "frontend");
        assert_eq!(frontend.command, "npm");
        assert_eq!(frontend.args, ["run", "dev"]);
        assert_eq!(
            frontend.working_dir.as_deref(),
            Some(Path::new("apps/frontend"))
        );
        assert!(!frontend.autostart);
        assert_eq!(
            frontend.env.get("NODE_ENV").map(String::as_str),
            Some("development")
        );
        assert_eq!(frontend.env.get("PORT").map(String::as_str), Some("4100"));

        let api = &cfg.servers[1];
        assert_eq!(api.name, "api");
        assert_eq!(api.command, "cargo");
        assert!(api.args.is_empty());
        assert!(api.working_dir.is_none());
        assert!(api.env.is_empty());
        assert!(
            api.autostart,
            "autostart should default to true when omitted"
        );
    }

    #[test]
    fn load_or_default_returns_empty_when_missing() {
        let missing_path = fixture_path("test-data/global-config/does-not-exist.toml");
        let cfg = load_or_default(missing_path);
        assert!(
            cfg.servers.is_empty(),
            "missing config should fall back to empty server list"
        );
    }

    #[test]
    fn load_parses_project_tasks() {
        let cfg = load(fixture_path("test-data/simple-project/flow.toml"))
            .expect("simple project config should parse");

        assert!(cfg.servers.is_empty(), "project fixture focuses on tasks");
        assert_eq!(cfg.tasks.len(), 2);

        let lint = &cfg.tasks[0];
        assert_eq!(lint.name, "lint");
        assert_eq!(lint.command, "golangci-lint run");
        assert_eq!(
            lint.description.as_deref(),
            Some("Run static analysis for Go sources")
        );

        let test_task = &cfg.tasks[1];
        assert_eq!(test_task.name, "test");
        assert_eq!(test_task.command, "gotestsum -f pkgname ./...");
        assert_eq!(
            test_task.description.as_deref(),
            Some("Execute the Go test suite with gotestsum output"),
            "desc alias should populate description"
        );
    }
}
