use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use shellexpand::tilde;

/// Top-level configuration for flowd, currently focused on managed servers.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub version: Option<u32>,
    /// Optional human-friendly project name (applies to local project configs).
    #[serde(
        default,
        rename = "name",
        alias = "project_name",
        alias = "project-name"
    )]
    pub project_name: Option<String>,
    #[serde(default)]
    pub options: OptionsConfig,
    #[serde(default, alias = "server", alias = "server-local")]
    pub servers: Vec<ServerConfig>,
    #[serde(default, rename = "server-remote")]
    pub remote_servers: Vec<RemoteServerConfig>,
    #[serde(default)]
    pub tasks: Vec<TaskConfig>,
    #[serde(default, alias = "deps")]
    pub dependencies: HashMap<String, DependencySpec>,
    #[serde(default, alias = "alias", deserialize_with = "deserialize_aliases")]
    pub aliases: HashMap<String, String>,
    #[serde(default, rename = "commands")]
    pub command_files: Vec<CommandFileConfig>,
    #[serde(default)]
    pub storage: Option<StorageConfig>,
    #[serde(default)]
    pub flox: Option<FloxConfig>,
    #[serde(default, alias = "watcher", alias = "always-run")]
    pub watchers: Vec<WatcherConfig>,
    #[serde(default)]
    pub stream: Option<StreamConfig>,
    #[serde(default, rename = "server-hub")]
    pub server_hub: Option<ServerHubConfig>,
    /// Background daemons that flow can manage (start/stop/status).
    #[serde(default, alias = "daemon")]
    pub daemons: Vec<DaemonConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: None,
            project_name: None,
            options: OptionsConfig::default(),
            servers: Vec::new(),
            remote_servers: Vec::new(),
            tasks: Vec::new(),
            dependencies: HashMap::new(),
            aliases: HashMap::new(),
            command_files: Vec::new(),
            storage: None,
            flox: None,
            watchers: Vec::new(),
            stream: None,
            server_hub: None,
            daemons: Vec::new(),
        }
    }
}

/// Global feature toggles.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OptionsConfig {
    #[serde(default, rename = "trace_terminal_io")]
    pub trace_terminal_io: bool,
}

impl OptionsConfig {
    fn merge(&mut self, other: OptionsConfig) {
        if other.trace_terminal_io {
            self.trace_terminal_io = true;
        }
    }
}

/// Configuration for a single managed HTTP server process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Human-friendly name used in the TUI and HTTP API.
    pub name: String,
    /// Program to execute, e.g. "node", "cargo".
    pub command: String,
    /// Arguments passed to the command.
    pub args: Vec<String>,
    /// Optional port the server listens on (for display only).
    pub port: Option<u16>,
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
            #[serde(default)]
            port: Option<u16>,
            #[serde(default, alias = "path")]
            working_dir: Option<String>,
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
                    Path::new(dir)
                        .file_name()
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

        let command = expand_path(&command).to_string_lossy().into_owned();

        Ok(ServerConfig {
            name,
            command,
            args,
            port: raw.port,
            working_dir: raw.working_dir.map(|dir| expand_path(&dir)),
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
    /// Whether this task should be handed off to the hub daemon instead of running locally.
    #[serde(default, rename = "delegate-to-hub", alias = "delegate_to_hub")]
    pub delegate_to_hub: bool,
    /// Whether this task should run automatically when entering the project root.
    #[serde(default)]
    pub activate_on_cd_to_root: bool,
    /// Optional task-specific dependencies that must be made available before the command runs.
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// Optional human-friendly description.
    #[serde(default, alias = "desc")]
    pub description: Option<String>,
    /// Optional short aliases that `f run` should recognize (e.g. "dcr" for "deploy-cli-release").
    #[serde(
        default,
        alias = "shortcut",
        alias = "short",
        deserialize_with = "deserialize_shortcuts"
    )]
    pub shortcuts: Vec<String>,
    /// Whether this task requires interactive input (stdin passthrough, TTY).
    #[serde(default)]
    pub interactive: bool,
    /// Require confirmation when matched via LM Studio (for destructive tasks).
    #[serde(default, alias = "confirm-on-match")]
    pub confirm_on_match: bool,
}

/// Definition of a dependency that can be referenced by automation tasks.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DependencySpec {
    /// Single command/binary that should be available on PATH.
    Single(String),
    /// Multiple commands that should be checked together.
    Multiple(Vec<String>),
    /// Flox package descriptor that should be added to the local env manifest.
    Flox(FloxInstallSpec),
}

fn deserialize_shortcuts<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ShortcutField {
        Single(String),
        Multiple(Vec<String>),
    }

    let value = Option::<ShortcutField>::deserialize(deserializer)?;
    let shortcuts = match value {
        Some(ShortcutField::Single(alias)) => vec![alias],
        Some(ShortcutField::Multiple(aliases)) => aliases,
        None => Vec::new(),
    };
    Ok(shortcuts)
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

/// Flox manifest-style configuration (install set, etc.).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FloxConfig {
    #[serde(default, rename = "install", alias = "deps")]
    pub install: HashMap<String, FloxInstallSpec>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FloxInstallSpec {
    #[serde(rename = "pkg-path")]
    pub pkg_path: String,
    #[serde(default, rename = "pkg-group")]
    pub pkg_group: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub systems: Option<Vec<String>>,
    #[serde(default)]
    pub priority: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommandFileConfig {
    pub path: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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
    #[serde(default)]
    pub driver: WatcherDriver,
    pub name: String,
    pub path: String,
    #[serde(default, rename = "match")]
    pub filter: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default)]
    pub run_on_start: bool,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub poltergeist: Option<PoltergeistConfig>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WatcherDriver {
    Shell,
    Poltergeist,
}

impl Default for WatcherDriver {
    fn default() -> Self {
        WatcherDriver::Shell
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoltergeistConfig {
    #[serde(default = "default_poltergeist_binary")]
    pub binary: String,
    #[serde(default)]
    pub mode: PoltergeistMode,
    #[serde(default)]
    pub args: Vec<String>,
}

impl Default for PoltergeistConfig {
    fn default() -> Self {
        Self {
            binary: default_poltergeist_binary(),
            mode: PoltergeistMode::Haunt,
            args: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PoltergeistMode {
    Haunt,
    Panel,
    Status,
}

impl Default for PoltergeistMode {
    fn default() -> Self {
        PoltergeistMode::Haunt
    }
}

fn default_debounce_ms() -> u64 {
    200
}

fn default_poltergeist_binary() -> String {
    "poltergeist".to_string()
}

impl PoltergeistMode {
    pub fn as_subcommand(&self) -> &'static str {
        match self {
            PoltergeistMode::Haunt => "haunt",
            PoltergeistMode::Panel => "panel",
            PoltergeistMode::Status => "status",
        }
    }
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

/// Configuration for a background daemon that flow can manage.
///
/// Example in flow.toml:
/// ```toml
/// [[daemon]]
/// name = "lin"
/// binary = "lin"
/// command = "daemon"
/// args = ["--host", "127.0.0.1", "--port", "9050"]
/// health_url = "http://127.0.0.1:9050/health"
///
/// [[daemon]]
/// name = "base"
/// binary = "base"
/// command = "jazz"
/// args = ["--port", "7201"]
/// health_url = "http://127.0.0.1:7201/health"
/// working_dir = "~/org/1f/base"
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct DaemonConfig {
    /// Unique name for this daemon (used in `f daemon start <name>`).
    pub name: String,
    /// Binary to execute (can be a name on PATH or absolute path).
    pub binary: String,
    /// Subcommand to run the daemon (e.g., "daemon", "jazz", "serve").
    #[serde(default)]
    pub command: Option<String>,
    /// Additional arguments passed after the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Health check URL to determine if daemon is running.
    #[serde(default, alias = "health")]
    pub health_url: Option<String>,
    /// Port the daemon listens on (extracted from health_url if not specified).
    #[serde(default)]
    pub port: Option<u16>,
    /// Host the daemon binds to.
    #[serde(default)]
    pub host: Option<String>,
    /// Working directory for the daemon process.
    #[serde(default, alias = "path")]
    pub working_dir: Option<String>,
    /// Environment variables to set for the daemon.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Whether to start this daemon automatically when flow starts.
    #[serde(default)]
    pub autostart: bool,
    /// Description of what this daemon does.
    #[serde(default)]
    pub description: Option<String>,
}

impl DaemonConfig {
    /// Get the effective health URL, building from host/port if not specified.
    pub fn effective_health_url(&self) -> Option<String> {
        if let Some(url) = &self.health_url {
            return Some(url.clone());
        }
        let host = self.host.as_deref().unwrap_or("127.0.0.1");
        self.port.map(|p| format!("http://{}:{}/health", host, p))
    }

    /// Get the effective host.
    pub fn effective_host(&self) -> &str {
        self.host.as_deref().unwrap_or("127.0.0.1")
    }
}

impl DependencySpec {
    /// Add one or more command names to the provided buffer.
    pub fn extend_commands(&self, buffer: &mut Vec<String>) {
        match self {
            DependencySpec::Single(cmd) => buffer.push(cmd.clone()),
            DependencySpec::Multiple(cmds) => buffer.extend(cmds.iter().cloned()),
            DependencySpec::Flox(_) => {}
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

/// Default config path: ~/.config/flow/flow.toml (falls back to legacy config.toml)
pub fn default_config_path() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/flow");

    let primary = base.join("flow.toml");
    if primary.exists() {
        return primary;
    }

    let legacy = base.join("config.toml");
    if legacy.exists() {
        tracing::warn!("using legacy config path ~/.config/flow/config.toml; rename to flow.toml");
        return legacy;
    }

    primary
}

pub fn expand_path(raw: &str) -> PathBuf {
    let tilde_expanded = tilde(raw).into_owned();
    let env_expanded = match shellexpand::env(&tilde_expanded) {
        Ok(val) => val.into_owned(),
        Err(_) => tilde_expanded,
    };
    PathBuf::from(env_expanded)
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
        if let Some(description) = include.description.as_deref() {
            tracing::debug!(
                path = %include_path.display(),
                description,
                "loading additional command file"
            );
        }
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
    if base.project_name.is_none() {
        base.project_name = other.project_name;
    }
    base.options.merge(other.options);
    base.servers.extend(other.servers);
    base.remote_servers.extend(other.remote_servers);
    base.tasks.extend(other.tasks);
    base.watchers.extend(other.watchers);
    base.daemons.extend(other.daemons);
    base.stream = base.stream.take().or(other.stream);
    base.storage = base.storage.take().or(other.storage);
    base.server_hub = base.server_hub.take().or(other.server_hub);
    for (key, value) in other.aliases {
        base.aliases.entry(key).or_insert(value);
    }
    for (key, value) in other.dependencies {
        base.dependencies.entry(key).or_insert(value);
    }
    match (&mut base.flox, other.flox) {
        (Some(base_flox), Some(other_flox)) => {
            for (key, value) in other_flox.install {
                base_flox.install.entry(key).or_insert(value);
            }
        }
        (None, Some(other_flox)) => base.flox = Some(other_flox),
        _ => {}
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
    use std::path::PathBuf;

    fn fixture_path(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
    }

    #[test]
    fn load_parses_global_fixture() {
        let cfg = load(fixture_path("test-data/global-config/flow.toml"))
            .expect("global config fixture should parse");

        assert_eq!(cfg.version, Some(1));
        assert!(cfg.options.trace_terminal_io, "options table should parse");
        assert_eq!(cfg.servers.len(), 1);
        assert_eq!(cfg.remote_servers.len(), 1);
        assert_eq!(cfg.watchers.len(), 1);
        assert_eq!(
            cfg.tasks.len(),
            1,
            "global config should inherit tasks from included command files"
        );

        let watcher = &cfg.watchers[0];
        assert_eq!(watcher.driver, WatcherDriver::Shell);
        assert_eq!(watcher.name, "karabiner");
        assert_eq!(watcher.path, "~/config/i/karabiner");
        assert_eq!(watcher.filter.as_deref(), Some("karabiner.edn"));
        assert_eq!(watcher.command.as_deref(), Some("~/bin/goku"));
        assert_eq!(watcher.debounce_ms, 150);
        assert!(watcher.run_on_start);
        assert!(watcher.poltergeist.is_none());

        let server = &cfg.servers[0];
        assert_eq!(server.name, "1f");
        assert_eq!(server.command, "blade");
        assert_eq!(server.args, ["--port", "4000"]);
        let working_dir = server
            .working_dir
            .as_ref()
            .expect("server working dir should parse");
        assert!(
            working_dir.ends_with("src/org/1f/1f"),
            "unexpected working dir: {}",
            working_dir.display()
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
    fn server_port_is_preserved_when_present() {
        let toml = r#"
            [[server]]
            name = "api"
            command = "npm start"
            port = 8080
        "#;

        let cfg: Config = toml::from_str(toml).expect("server config should parse");
        let server = cfg.servers.first().expect("server should parse");
        assert_eq!(server.port, Some(8080));

        // Missing port should deserialize as None for backward compatibility.
        let no_port_toml = r#"
            [[server]]
            name = "web"
            command = "npm run dev"
        "#;
        let cfg: Config =
            toml::from_str(no_port_toml).expect("server config without port should parse");
        assert_eq!(cfg.servers[0].port, None);
    }

    #[test]
    fn expand_path_supports_tilde_and_env() {
        let home = std::env::var("HOME").expect("HOME must be set for tests");
        let expected = PathBuf::from(&home).join("projects/demo");

        assert_eq!(expand_path("~/projects/demo"), expected);
        assert_eq!(expand_path("$HOME/projects/demo"), expected);
    }

    #[test]
    fn parses_poltergeist_watcher() {
        let toml = r#"
            [[watchers]]
            driver = "poltergeist"
            name = "peekaboo"
            path = "~/src/org/1f/peekaboo"

            [watchers.poltergeist]
            binary = "/opt/bin/poltergeist"
            mode = "panel"
            args = ["status", "--verbose"]
        "#;

        let cfg: Config = toml::from_str(toml).expect("poltergeist watcher should parse");
        assert_eq!(cfg.watchers.len(), 1);
        let watcher = &cfg.watchers[0];
        assert_eq!(watcher.driver, WatcherDriver::Poltergeist);
        assert_eq!(watcher.command, None);
        assert_eq!(watcher.path, "~/src/org/1f/peekaboo");

        let poltergeist = watcher
            .poltergeist
            .as_ref()
            .expect("poltergeist config should exist");
        assert_eq!(poltergeist.binary, "/opt/bin/poltergeist");
        assert_eq!(poltergeist.mode, PoltergeistMode::Panel);
        assert_eq!(poltergeist.args, vec!["status", "--verbose"]);
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
    fn parses_flox_dependencies_and_config() {
        let contents = r#"
[dependencies]
rg.pkg-path = "ripgrep"

[flox.deps]
fd.pkg-path = "fd"
"#;

        let cfg: Config = toml::from_str(contents).expect("config with flox deps should parse");

        match cfg.dependencies.get("rg") {
            Some(DependencySpec::Flox(spec)) => {
                assert_eq!(spec.pkg_path, "ripgrep");
            }
            other => panic!("unexpected dependency variant: {other:?}"),
        }

        let flox = cfg.flox.expect("flox config should exist");
        let fd = flox
            .install
            .get("fd")
            .expect("fd install should be present");
        assert_eq!(fd.pkg_path, "fd");
    }

    #[test]
    fn task_activation_flag_defaults_and_parses() {
        let toml = r#"
[[tasks]]
name = "lint"
command = "golangci-lint run"

[[tasks]]
name = "setup"
command = "cargo check"
activate_on_cd_to_root = true
"#;

        let cfg: Config = toml::from_str(toml).expect("activation config should parse");
        assert_eq!(cfg.tasks.len(), 2);
        assert!(!cfg.tasks[0].activate_on_cd_to_root);
        assert!(cfg.tasks[1].activate_on_cd_to_root);
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

    #[test]
    fn options_defaults_are_false() {
        let cfg: Config =
            toml::from_str("").expect("empty config should parse with default options");
        assert!(!cfg.options.trace_terminal_io);
    }

    #[test]
    fn options_trace_flag_parses() {
        let toml = r#"
[options]
trace_terminal_io = true
"#;
        let cfg: Config = toml::from_str(toml).expect("options table should parse");
        assert!(cfg.options.trace_terminal_io);
    }
}
