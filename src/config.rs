use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer};

/// Top-level configuration for flowd, currently focused on managed servers.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub version: Option<u32>,
    #[serde(default, alias = "server", alias = "server-local")]
    pub servers: Vec<ServerConfig>,
    #[serde(default, rename = "server-remote")]
    pub remote_servers: Vec<RemoteServerConfig>,
    #[serde(default)]
    pub tasks: Vec<TaskConfig>,
    #[serde(default)]
    pub dependencies: HashMap<String, DependencySpec>,
    #[serde(default, alias = "alias", deserialize_with = "deserialize_aliases")]
    pub aliases: HashMap<String, String>,
    #[serde(default, rename = "commands")]
    pub command_files: Vec<CommandFileConfig>,
    #[serde(default)]
    pub storage: Option<StorageConfig>,
    #[serde(default, alias = "watcher", alias = "always-run")]
    pub watchers: Vec<WatcherConfig>,
    #[serde(default)]
    pub stream: Option<StreamConfig>,
    #[serde(default, rename = "server-hub")]
    pub server_hub: Option<ServerHubConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: None,
            servers: Vec::new(),
            remote_servers: Vec::new(),
            tasks: Vec::new(),
            dependencies: HashMap::new(),
            aliases: HashMap::new(),
            command_files: Vec::new(),
            storage: None,
            watchers: Vec::new(),
            stream: None,
            server_hub: None,
        }
    }
}

/// Configuration for a single managed HTTP server process.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Human-friendly name used in the TUI and HTTP API.
    pub name: String,
    /// Program to execute, e.g. "node", "cargo".
    pub command: String,
    /// Arguments passed to the command.
    pub args: Vec<String>,
    /// Optional working directory for the process.
    pub working_dir: Option<PathBuf>,
    /// Additional environment variables.
    pub env: HashMap<String, String>,
    /// Whether this server should be started automatically with the daemon.
    pub autostart: bool,
}

impl<'de> Deserialize<'de> for ServerConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawServerConfig {
            #[serde(default)]
            name: Option<String>,
            command: String,
            #[serde(default)]
            args: Vec<String>,
            #[serde(default, alias = "path")]
            working_dir: Option<PathBuf>,
            #[serde(default)]
            env: HashMap<String, String>,
            #[serde(default = "default_autostart")]
            autostart: bool,
        }

        let raw = RawServerConfig::deserialize(deserializer)?;
        let mut command = raw.command;
        let mut args = raw.args;

        if args.is_empty() {
            if let Ok(parts) = shell_words::split(&command) {
                if let Some((head, tail)) = parts.split_first() {
                    command = head.clone();
                    args = tail.to_vec();
                }
            }
        }

        let name = raw
            .name
            .or_else(|| {
                raw.working_dir.as_ref().and_then(|dir| {
                    dir.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .filter(|s| !s.is_empty())
                })
            })
            .unwrap_or_else(|| {
                if command.is_empty() {
                    "server".to_string()
                } else {
                    command.clone()
                }
            });

        Ok(ServerConfig {
            name,
            command,
            args,
            working_dir: raw.working_dir,
            env: raw.env,
            autostart: raw.autostart,
        })
    }
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
    /// Optional task-specific dependencies that must be made available before the command runs.
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// Optional human-friendly description.
    #[serde(default, alias = "desc")]
    pub description: Option<String>,
}

/// Definition of a dependency that can be referenced by automation tasks.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DependencySpec {
    /// Single command/binary that should be available on PATH.
    Single(String),
    /// Multiple commands that should be checked together.
    Multiple(Vec<String>),
}

/// Storage configuration describing remote environments providers.
#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    /// Provider identifier understood by the hosted hub.
    pub provider: String,
    /// Environment variable that stores the API key/token.
    #[serde(default = "default_storage_env_var")]
    pub env_var: String,
    /// Base URL for the storage hub (defaults to hosted flow hub).
    #[serde(default = "default_hub_url")]
    pub hub_url: String,
    /// Environments that can be synced locally.
    #[serde(default)]
    pub envs: Vec<StorageEnvConfig>,
}

fn default_hub_url() -> String {
    "https://flow.1focus.ai".to_string()
}

fn default_storage_env_var() -> String {
    "FLOW_SECRETS_TOKEN".to_string()
}

/// Definition of an environment with named variables.
#[derive(Debug, Clone, Deserialize)]
pub struct StorageEnvConfig {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub variables: Vec<StorageVariable>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageVariable {
    pub key: String,
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommandFileConfig {
    pub path: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteServerConfig {
    #[serde(flatten)]
    pub server: ServerConfig,
    /// Optional hub name that coordinates this remote process.
    #[serde(default)]
    pub hub: Option<String>,
    /// Paths to sync to the remote hub before launching.
    #[serde(default)]
    pub sync_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerHubConfig {
    pub name: String,
    pub host: String,
    #[serde(default = "default_server_hub_port")]
    pub port: u16,
    #[serde(default)]
    pub tailscale: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

fn default_server_hub_port() -> u16 {
    9050
}

/// File watcher configuration for local automation.
#[derive(Debug, Clone, Deserialize)]
pub struct WatcherConfig {
    pub name: String,
    pub path: String,
    #[serde(default, rename = "match")]
    pub filter: Option<String>,
    pub command: String,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default)]
    pub run_on_start: bool,
}

fn default_debounce_ms() -> u64 {
    200
}

/// Streaming configuration handled by the hub (stub for future OBS integration).
#[derive(Debug, Clone, Deserialize)]
pub struct StreamConfig {
    pub provider: String,
    #[serde(default)]
    pub hotkey: Option<String>,
    #[serde(default)]
    pub toggle_url: Option<String>,
}

impl DependencySpec {
    /// Add one or more command names to the provided buffer.
    pub fn extend_commands(&self, buffer: &mut Vec<String>) {
        match self {
            DependencySpec::Single(cmd) => buffer.push(cmd.clone()),
            DependencySpec::Multiple(cmds) => buffer.extend(cmds.iter().cloned()),
        }
    }
}

fn deserialize_aliases<'de, D>(deserializer: D) -> Result<HashMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum AliasInput {
        Map(HashMap<String, String>),
        List(Vec<HashMap<String, String>>),
    }

    let maybe = Option::<AliasInput>::deserialize(deserializer)?;
    let mut aliases = HashMap::new();
    if let Some(input) = maybe {
        match input {
            AliasInput::Map(map) => aliases = map,
            AliasInput::List(list) => {
                for table in list {
                    for (name, command) in table {
                        aliases.insert(name, command);
                    }
                }
            }
        }
    }

    Ok(aliases)
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
    let mut visited = Vec::new();
    load_with_includes(path, &mut visited)
}

fn load_with_includes(path: &Path, visited: &mut Vec<PathBuf>) -> Result<Config> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to resolve path {}", path.display()))?;
    if visited.contains(&canonical) {
        anyhow::bail!(
            "cycle detected while loading config includes: {}",
            path.display()
        );
    }
    visited.push(canonical.clone());

    let contents = fs::read_to_string(&canonical)
        .with_context(|| format!("failed to read flow config at {}", path.display()))?;
    let mut cfg: Config = toml::from_str(&contents)
        .with_context(|| format!("failed to parse flow config at {}", path.display()))?;

    for include in cfg.command_files.clone() {
        let include_path = resolve_include_path(&canonical, &include.path);
        let included = load_with_includes(&include_path, visited)
            .with_context(|| format!("failed to load commands file {}", include_path.display()))?;
        merge_config(&mut cfg, included);
    }

    visited.pop();
    Ok(cfg)
}

fn resolve_include_path(base: &Path, include: &str) -> PathBuf {
    let include_path = PathBuf::from(include);
    if include_path.is_absolute() {
        include_path
    } else if let Some(parent) = base.parent() {
        parent.join(include_path)
    } else {
        include_path
    }
}

fn merge_config(base: &mut Config, other: Config) {
    base.servers.extend(other.servers);
    base.remote_servers.extend(other.remote_servers);
    base.tasks.extend(other.tasks);
    base.watchers.extend(other.watchers);
    base.stream = base.stream.take().or(other.stream);
    base.storage = base.storage.take().or(other.storage);
    base.server_hub = base.server_hub.take().or(other.server_hub);
    for (key, value) in other.aliases {
        base.aliases.entry(key).or_insert(value);
    }
    for (key, value) in other.dependencies {
        base.dependencies.entry(key).or_insert(value);
    }
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

        assert_eq!(cfg.version, Some(1));
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.remote_servers.len(), 1);
        assert_eq!(cfg.watchers.len(), 1);
        assert_eq!(
            cfg.tasks.len(),
            1,
            "global config should inherit tasks from included command files"
        );

        let watcher = &cfg.watchers[0];
        assert_eq!(watcher.name, "karabiner");
        assert_eq!(watcher.path, "~/config/i/karabiner");
        assert_eq!(watcher.filter.as_deref(), Some("karabiner.edn"));
        assert_eq!(watcher.command, "~/bin/goku");
        assert_eq!(watcher.debounce_ms, 150);
        assert!(watcher.run_on_start);

        let server = &cfg.servers[0];
        assert_eq!(server.name, "1f");
        assert_eq!(server.command, "blade");
        assert_eq!(server.args, ["--port", "4000"]);
        assert_eq!(
            server.working_dir.as_deref(),
            Some(Path::new("~/src/org/1f/1f"))
        );
        assert!(server.env.is_empty());
        assert!(
            server.autostart,
            "autostart should default to true when omitted"
        );

        let sync_task = &cfg.tasks[0];
        assert_eq!(sync_task.name, "sync-config");
        assert_eq!(
            sync_task.command,
            "rsync -av ~/.config/flow remote:~/flow-config"
        );
        assert!(
            cfg.aliases.contains_key("fsh"),
            "included aliases should merge into base config"
        );

        let remote = &cfg.remote_servers[0];
        assert_eq!(remote.server.name, "homelab-blade");
        assert_eq!(remote.hub.as_deref(), Some("homelab"));
        assert_eq!(remote.sync_paths, [PathBuf::from("~/config/i/karabiner")]);

        let hub = cfg.server_hub.as_ref().expect("server hub config");
        assert_eq!(hub.name, "homelab");
        assert_eq!(hub.host, "tailscale");
        assert_eq!(hub.port, 9050);
        assert_eq!(hub.tailscale.as_deref(), Some("linux-hub"));
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

    #[test]
    fn load_parses_dependency_table() {
        let contents = r#"
[dependencies]
fast = "fast"
toolkit = ["rg", "fd"]

[[tasks]]
name = "ci"
command = "ci"
dependencies = ["fast", "toolkit"]
"#;
        let cfg: Config =
            toml::from_str(contents).expect("inline config with dependencies should parse");

        let task = cfg.tasks.first().expect("task should parse");
        assert_eq!(
            task.dependencies,
            ["fast", "toolkit"],
            "task dependency references should parse"
        );

        let fast = cfg
            .dependencies
            .get("fast")
            .expect("fast dependency should be present");
        match fast {
            DependencySpec::Single(expr) => {
                assert_eq!(expr, "fast");
            }
            other => panic!("fast dependency variant mismatch: {other:?}"),
        }

        let toolkit = cfg
            .dependencies
            .get("toolkit")
            .expect("toolkit dependency should be present");
        match toolkit {
            DependencySpec::Multiple(exprs) => {
                assert_eq!(exprs, &["rg", "fd"]);
            }
            other => panic!("toolkit dependency variant mismatch: {other:?}"),
        }
    }

    #[test]
    fn load_parses_aliases() {
        let contents = r#"
[aliases]
fr = "f run"
ls = "f tasks"
"#;
        let cfg: Config = toml::from_str(contents).expect("inline alias config should parse");
        assert_eq!(cfg.aliases.get("fr").map(String::as_str), Some("f run"));
        assert_eq!(cfg.aliases.get("ls").map(String::as_str), Some("f tasks"));
    }

    #[test]
    fn load_parses_alias_array_table() {
        let contents = r#"
[[alias]]
fr = "f run"
fc = "f commit"

[[alias]]
dev = "f run dev"
"#;
        let cfg: Config = toml::from_str(contents).expect("alias array config should parse");
        assert_eq!(cfg.aliases.get("fr").map(String::as_str), Some("f run"));
        assert_eq!(cfg.aliases.get("fc").map(String::as_str), Some("f commit"));
        assert_eq!(
            cfg.aliases.get("dev").map(String::as_str),
            Some("f run dev")
        );
    }
}
